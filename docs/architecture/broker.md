# The broker: a modular microkernel for terminus-rs tools

This document describes the modular broker architecture built across
TMOD-01..06 (all merged to `main`) and lays out the repeatable runbook for
extracting one of the remaining domain modules (`src/<domain>/`, registered
via `register_all` in `src/registry.rs`) into an out-of-process worker.

Audience: the operator, plus contributors/agents who will run the ~28
follow-on domain extractions this design was built to support.

## 1. Overview: broker vs. workers

Before TMOD-01..06, `terminus-rs` was a single, monolithic tool registry:
every domain module (`plane`, `gitea`, `vitals`, `ledger`, …) was compiled
directly into the crate and registered into one in-process `HashMap` at
startup (`crate::registry::register_all`). That is still true for most
domains today — the broker doesn't replace this, it adds a second, optional
path alongside it.

The **broker** is the process that already embeds the compiled-in registry
(the gateway/primary binary — see `src/mcp_server.rs`'s `McpServerState`). A
**worker** is a separate OS process that owns one domain's tools and is
reached over an in-box transport (Unix domain socket, optionally with mTLS)
rather than an in-process function call. The broker's dispatcher tries the
compiled-in registry first, then a dynamic route table of worker-owned
tools, before falling through to personal-federation or "Unknown tool".

This is a microkernel split, and it buys four things a monolithic registry
cannot:

- **Hot add/update.** A worker registers, updates, or is replaced via the
  admin control plane (`src/broker/control.rs`) with no broker restart and
  no interruption to any other domain's tools — the broker process's own
  binary doesn't change.
- **Least-privilege secret isolation.** A worker process only holds the
  secrets its own domain needs (e.g. a `gitea` worker holds a Gitea token,
  nothing else). A compromised worker cannot read another domain's
  credentials, because they were never in its process's environment or
  memory to begin with — a much stronger boundary than "one big process with
  every domain's secrets in `SecretManager`".
- **Fault isolation.** A wedged, crashed, or slow worker fails only its own
  tools. Every dispatch and catalog path in this design is built around that
  invariant — see §3 (bounded health probes) and §5 (rollout) below — and it
  is directly tested (`RouteTable`'s "one dead worker only fails its own
  tools" test suite).
- **Independent evolution.** A worker crate can be built, versioned,
  deployed, and rolled back independently of `terminus-rs` itself and of
  every other worker. It depends on `terminus-worker-sdk` (which re-exports
  the same `RustTool`/`ToolOutput`/`ToolError`/`ToolInfo` authoring types the
  compiled-in registry uses) but not on the rest of the monolith.

None of this is mandatory per-domain: a domain stays compiled-in until
someone deliberately extracts it (§6). The broker's dispatch precedence
(§2) is specifically designed so compiled-in and worker-routed tools are
indistinguishable to a caller, which is what makes migrating one domain at a
time — instead of a big-bang rewrite — safe.

## 2. Registry + route table

Two independent, atomically-swappable data structures sit behind every
request:

- **`McpServerState::registry`** (`src/registry.rs`) — an
  `arc_swap::ArcSwap<ToolRegistry>` wrapping the compiled-in tools built by
  `register_all`/`register_personal`. `ToolRegistry` is a simple
  name-keyed map; a whole fresh registry is built and swapped in as one
  `Arc`, so no individual tool's boxed state is ever deep-cloned mid-swap.
- **`McpServerState::broker_routes`** (`src/broker/routes.rs`) — a
  `RouteTable`, also an `ArcSwap<RouteTableSnapshot>`, mapping bare tool
  names to a `WorkerRoute { worker_id, transport, tool }`. Writers (the
  admin control plane, §4) go through `install`/`install_many`/
  `replace_worker`/`remove`/`remove_worker`, each an `ArcSwap::rcu`
  compare-and-swap retry loop over a copy-on-write snapshot — never an
  in-place mutation of a snapshot a reader already holds.

Both structures share the same read contract: `src/mcp_server.rs`'s
`handle_mcp` calls `.load()` **exactly once** at the top of a request and
dispatches the whole request against that one snapshot. A swap that lands
mid-request is never torn — an in-flight call sees either the table as it
stood when the request started, or (for a request that starts after the
swap) the new one, never a mix. This is the same pattern used twice
(TMOD-01 for the registry, TMOD-04 for the route table) deliberately, so the
two swappable structures behave identically from a caller's point of view.

**Dispatch precedence** — documented once here, enforced in
`src/mcp_server.rs`, and identical in both directions:

```
compiled-in registry  >  worker route table  >  personal-federation  >  "Unknown tool"
```

A `tools/call` whose name exists in *both* the compiled-in registry and the
route table always dispatches to the compiled-in tool; the route table is
consulted only on a compiled-in miss. `tools/list` applies the exact same
rule when merging catalogs (`crate::broker::routes::merge_catalog`): a name
present in both a compiled-in tool and a healthy worker's advertised
catalog is listed once, as the compiled-in tool. `tools/list` and
`tools/call` are guaranteed to never disagree about which source "owns" a
given tool name.

Practically: **extracting a domain to a worker only takes effect once the
domain is removed from `register_all`** (§6, step 6). Until then, the
compiled-in copy always wins and the worker's route is dead weight (never
reached, though it is still listed as available in worker admin output).

## 3. Transport tiers and the capability floor

A worker is reached over one of three `WorkerTransport` implementations
(`src/broker/transport/`), selected per worker by its declared
`TransportTier`:

| Tier | Module | On-host only? | Cryptographic identity? | Kernel peer attestation? |
|---|---|---|---|---|
| **T1** | `uds_peercred` | Yes (UDS) | No | Yes (`SO_PEERCRED`) |
| **T0** | `mtls_tcp` | No (TCP, may cross a host boundary) | Yes (mTLS) | No |
| **T2** (default/strongest) | `uds_mtls` | Yes (UDS) | Yes (mTLS) | Yes (`SO_PEERCRED`) |

Loopback (`127.0.0.1`) is **never** treated as a trust boundary anywhere in
this module — a tier either stays entirely inside the kernel (a UDS, whose
peer identity the kernel itself attests) or authenticates cryptographically
(mTLS), never "it came from localhost so it must be us".

- **T1** is the weakest tier this module offers: same-host only, no
  cryptographic identity, appropriate only for low-risk, read-only tools
  where kernel peer-uid attestation is judged sufficient.
- **T0** is for a worker that is genuinely off-box (not reachable via a
  shared filesystem for a UDS). It is cryptographically authenticated like
  T2 but network-exposed, so it ranks *between* T1 and T2, not equal to
  either.
- **T2** requires BOTH independent identity signals — the kernel-attested
  `SO_PEERCRED` peer uid and the TLS peer leaf certificate's Subject CN — to
  agree with the worker's *configured* identity. Either one disagreeing is a
  fail-closed rejection (never "agree with each other", always "agree with
  config").

`TransportTier::security_rank()` orders these T1 < T0 < T2 for floor
comparisons — deliberately **not** the enum's declaration order, so a future
refactor can't accidentally derive `Ord` from declaration order and silently
invert the security ranking.

**`MinTierPolicy`** maps a worker's declared `CapabilityClass` to the lowest
tier it may register at. The Rust enum variants are `ReadOnly` /
`WriteScoped` / `SecretHolding`, but the **canonical string form** — the one
you write in a registration manifest and in `TERMINUS_BROKER_WORKERS_JSON`
config, and the exact form `MinTierPolicy` deserializes and evaluates — is
**snake_case** (`#[serde(rename_all = "snake_case")]`): `read_only`,
`write_scoped`, `secret_holding`. Use those exact snake_case strings
everywhere a `capability_class` *value* is written; the rest of this doc
does. (Note this is a different field from `terminus-worker-sdk`'s
`Worker::capability_class` builder, which is a free-form coarse hint that
defaults to `"core"` and is NOT what `MinTierPolicy` evaluates — see §6.)

| `capability_class` value | Minimum tier |
|---|---|
| `read_only` | T1 |
| `write_scoped` | T2 |
| `secret_holding` | T2 |

A worker declared `write_scoped` or `secret_holding` registering below T2 is
rejected — enforced both at static config-load time
(`crate::config::validate_worker_transport_entry`) and again, independently,
inside the admin control plane's registration handler, so a future caller
that constructs a transport directly (bypassing config) still can't
silently under-provision a sensitive worker.

**Picking a tier when extracting a domain:** classify the domain's actual
side effects, not its name. A domain that only reads and never mutates
external state or touches a credential is `read_only`/T1. A domain that
calls a mutating API (create/update/delete against Gitea, Plane, GitHub,
etc.) is `write_scoped`. A domain that holds or transmits secret material
(API tokens, private keys) is `secret_holding`. When in doubt, or when a
domain mixes both, treat it as `secret_holding`/T2 — the floor exists
precisely so a misclassification defaults to the safer side.

## 4. The admin control plane

Before TMOD-05, a worker could be dialed and routed to *in code*, but
nothing on any live path ever mutated the route table — this is that live
path: a small, authenticated HTTP admin surface (`src/broker/control.rs`)
mounted on the control surface, deliberately **never** on the public `/mcp`
router (they share no route prefix, so a caller reaching `/mcp` can never
accidentally hit `/admin/workers/*`).

Four endpoints:

- `POST /admin/workers/register` — onboard or update a worker.
- `POST /admin/workers/deregister` — remove a worker's routes entirely.
- `POST /admin/workers/health` — probe one or all registered workers
  (read-only; never mutates the route table).
- `GET /admin/workers` — list registered workers (id, tools, tier,
  capability class, last known health) with no secret material in the
  response.

### The pre-flip gate

Registration is deliberately conservative — a worker only gets a route once
it has proven itself, in order:

1. **Validate** the manifest (`crate::config::validate_worker_transport_entry`)
   — the same rule set static `TERMINUS_BROKER_WORKERS_JSON` config uses,
   including the `MinTierPolicy` floor from §3.
2. **Connect + bounded health probe.** `WorkerTransport::connect()` must
   succeed AND a bounded `health()` probe (same timeout budget every health
   check in this crate uses) must return `true`. A worker that fails either
   check is refused before any route is installed.
3. **`list()`-verified catalog.** The worker's advertised tool set is taken
   from a live `WorkerTransport::list()` call — this IS the
   initialize+catalog gate: it proves the worker actually speaks the wire
   protocol and reports what it truly serves. The routes installed come
   from this verified list, **never** from the (untrusted) registration
   request body — the body's tool entries only *enrich* a verified tool's
   catalog metadata (description/inputSchema) by name; a body-declared tool
   the worker doesn't actually serve never becomes a route. A worker whose
   `list()` fails, times out, or returns nothing is refused.

Only once all three pass does registration proceed to the blue-green flip
(§5).

### Kind-aware admin authz — NOT the ordinary tool grant

Every admin op is guarded through the same identity → allowlist →
rate-limit → audit pipeline `/mcp` uses (`GatewayFramework::guard`), but as
`ActionKind::Admin` with an `"admin:<op>"` action string. That means a
generic tool wildcard grant (e.g. `allow: ["*"]`) does **not** authorize any
admin op — an identity must hold an explicit admin-namespace grant
(`"admin:*"` or an exact `"admin:register_worker"`). This closes a
privilege-escalation gap where any broad tool/inference identity would
otherwise silently gain worker register/deregister (route-hijack) power.

Unlike `/mcp` (which stays usable, ungated, when no `GatewayFramework` is
configured — preserving pre-gating behavior for deployments that never
opted in), the admin control plane **never runs open**: no `GatewayFramework`
configured means no admin-auth secret is provisioned at all, and a process
in that state refuses every admin op rather than silently allowing them.
Fail-closed, always.

### Audit — name-only, never address/secret-shaped

Every handler records exactly one audit entry per operation, with a short,
name-only detail string (worker id, tier, capability class, tool count). A
failure audit logs only a fixed error *category* token
(`"connect_failed"`, `"health_timeout"`, `"catalog_unavailable"`, …), never
the error's raw `Display` string — which, for a transport/catalog failure,
can contain a worker host:port, UDS socket path, or cert-CN-mismatch
detail. A defense-in-depth sanitizer runs on that detail as a second layer.

## 5. Blue-green rollout and rollback

TMOD-05's registration alone was safe against a worker that was dead *at
registration time* (the pre-flip gate refuses it) but not against one that
passes the gate and then goes bad moments later — a build that boots,
answers one health probe, then wedges. TMOD-06 closes that gap.

**The flip** (`crate::broker::rollout::rollout_worker`) is one atomic swap
— `RouteTable::replace_worker_with_rollback` — that installs the new
instance's routes for `worker_id` AND hands back whatever routes it just
displaced (the worker's previous instance, or empty on a first-ever
registration) as this rollout's rollback state. Because the route table
never tears mid-request (§2), every in-flight call dispatches against
either the pre-flip snapshot (old instance) or a post-flip one (new
instance) — never both.

**The post-flip health window** runs a small, fixed number of consecutive
health probes (`POST_FLIP_HEALTH_CHECKS`, each individually bounded) against
the newly-flipped instance. This is deliberately "N of N", not "N of M": a
single failed probe anywhere in the window fails the whole rollout — a
worker that flaps during its own post-flip window is exactly the failure
this mechanism exists to catch, so treating a flap as a pass would defeat
the point.

**Rollback is generation-guarded, not value-guarded.** Each flip stamps a
strictly monotonic per-worker generation number (`worker_gen`). On a failed
window, `RouteTable::restore_worker_if_unchanged` restores the previous
instance's routes (or removes the routes entirely, if there was no previous
instance — fail-safe, never leave a route pointing at a proven-bad
instance) **only if** the worker's current owning generation still equals
the generation this rollout's flip stamped, checked in the same atomic
swap. This closes the ABA hole a routes-value or `Arc::ptr_eq` comparison
alone could not: a competing rollout could flip the worker away and a later
one flip it back to a byte-identical (even same-`Arc`) route set, which a
value/pointer comparison would wrongly accept as "unchanged" — but every
intervening flip draws a strictly-greater generation, so a stale rollback
token can never match again. A deregister drops the generation entry
entirely (also a mismatch, so an in-flight rollback of a since-deregistered
worker is a clean no-op).

A first-ever registration goes through this exact same path: if a
brand-new worker regresses during its post-flip window, it is **removed**
(there is no previous instance to fall back to) rather than left routed to
a proven-bad instance.

**The accepted trade-off:** this is a flip-*then*-verify model. A narrow
window exists where a call landing after the flip but before a rollback
completes can hit the just-regressed new instance. Fully avoiding this
would require dual-routing one tool name to both the old and new instance
simultaneously and reconciling their answers — the flat one-route-per-name
table cannot express that, and the trade-off is accepted: the window is
short, the pre-flip gate makes a healthy-at-flip instance the norm, and
rollback itself is atomic and fast.

## 6. Per-domain extraction runbook

Copy-pasteable checklist for turning one compiled-in `src/<domain>/` module
into an out-of-process worker. Do not skip steps or reorder step 6 earlier
than step 5 — deploying before removing from `register_all` is what keeps
the live gateway serving that domain's tools throughout the migration.

1. **KG-ground.** Before touching code, query the domain's Atlas knowledge
   graph entry (`kg_*` tools / `<path>/.atlas-graphs`, per this
   project's KG tooling) to confirm the domain's actual call graph, its
   external dependencies (which secrets/env vars it reads,
   `reqwest`/API clients it constructs), and whether any *other* compiled-in
   module calls into it directly (a hard dependency that would need
   resolving before extraction, not after).

2. **Scaffold a new `terminus-<domain>` crate on `terminus-worker-sdk`.**
   Add it as a new workspace member (see `Cargo.toml`'s `[workspace]
   members`, alongside the existing `terminus-client` and
   `terminus-worker-sdk` entries). Depend on `terminus-worker-sdk` (which
   re-exports `RustTool`/`ToolOutput`/`ToolError`/`ToolInfo` from
   `terminus-rs` by path — the authoring types are not relocated or
   duplicated). A minimal worker binary looks like:

   ```rust
   use terminus_worker_sdk::{RustTool, ToolError, Worker};
   use serde_json::Value;

   #[async_trait::async_trait]
   impl RustTool for MyDomainTool { /* ... */ }

   #[tokio::main]
   async fn main() -> Result<(), Box<dyn std::error::Error>> {
       // A Rust string literal does NOT expand env vars -- read the socket
       // path from the environment explicitly.
       let socket_path = std::env::var("TERMINUS_WORKER_SOCKET_PATH")?;
       Worker::builder("<domain>-worker", "0.1.0")
           // NOTE: this SDK builder field is a free-form COARSE hint
           // (defaults to "core") that goes in the worker's own manifest --
           // it is NOT the security-relevant `capability_class` MinTierPolicy
           // evaluates. That one (`read_only`/`write_scoped`/`secret_holding`,
           // see §3) is declared at REGISTRATION time in the admin manifest
           // (step 6), not here.
           .capability_class("core")
           .tool(Box::new(MyDomainTool))
           // one .tool(...) call per RustTool the domain owns
           .serve(&socket_path)
           .await?;
       Ok(())
   }
   ```

3. **Move the tool registration, keep the library.** Move the domain's
   `RustTool` impls (and the `register(registry: &mut ToolRegistry)`
   function's *call sites*, not the shared logic underneath if other code
   still needs it) into the new crate's `main.rs`/modules. If the domain's
   underlying client/logic (e.g. an HTTP client wrapper) is depended on
   elsewhere in `terminus-rs`, leave that library code in place in
   `src/<domain>/` and have the new worker crate depend on `terminus-rs` by
   path to reuse it (the same relationship `terminus-worker-sdk` itself
   has) — do not duplicate business logic across the monolith and the
   worker crate.

4. **Pick a tier by capability class**, per §3's table, using the canonical
   snake_case `capability_class` value the registration manifest expects.
   Read-only domain → `read_only`/T1. Anything that mutates external state or
   touches a credential → `write_scoped` (or `secret_holding` if it holds
   secret material) / T2. Default to `secret_holding`/T2 when unsure. (This
   is the value you put in the registration manifest at step 6, not the SDK
   builder's coarse hint from step 2.)

5. **Do NOT yet remove the domain from `register_all`** (`src/registry.rs`).
   Leave the compiled-in copy in place for now — per §2's precedence rule
   it will keep serving all traffic even after the worker is deployed and
   registered, since compiled-in always wins on a name collision. This is
   what makes the next two steps reversible with zero gateway downtime.

6. **Deploy the worker + register via the admin control plane.**
   - Build and deploy the new `terminus-<domain>` binary (see the systemd
     unit template below).
   - Call `POST /admin/workers/register` with the worker's transport
     manifest — `name`, `tier`, `capability_class` (the snake_case value
     from §3: `read_only` / `write_scoped` / `secret_holding`),
     `socket_path`/`expected_uid` for T1/T2, or `host`/`port` for T0, plus
     `expected_identity` for T0/T2 — and its declared tool list. Registration only succeeds once the
     pre-flip gate (§4) and the post-flip rollout window (§5) both pass —
     if it fails, the worker is removed/rolled back automatically and
     `register_all` still owns the tools, so nothing regresses.
   - Confirm with `GET /admin/workers` that the worker is listed with the
     expected tool set, tier, and `last_health: true`.

7. **Verify list/call/fault-isolation before touching `register_all`.**
   - `tools/list` on the broker still shows each colliding tool ONCE, owned
     by the compiled-in implementation: `merge_catalog` explicitly skips any
     worker route whose name collides with a compiled-in tool, so the worker's
     route does not appear as a duplicate `tools/list` entry (this matches the
     `tools/call` precedence — compiled-in still wins while it exists).
     Confirm the worker's route is registered via `GET /admin/workers`, not via
     a second `tools/list` entry.
   - Manually exercise `POST /admin/workers/health` and confirm the worker
     reports healthy.
   - Optionally, deliberately stop the worker process and confirm
     `GET /admin/workers` reflects it as unhealthy, and that this has zero
     effect on any *other* tool (fault isolation) — the point of this step
     is proving the worker doesn't destabilize the broker before it is
     load-bearing.

8. **Only now remove the domain's `register_all` call site** in
   `src/registry.rs` (and from `register_personal`, if applicable — check
   both; a domain present in both functions per the core/personal
   collision table needs both removed, or deliberately only one, depending
   on which surface(s) it should now be served on as a worker). This is the
   moment dispatch precedence hands the domain's tools to the worker route.
   Deploy the updated `terminus-rs` binary with the domain's compiled-in
   code removed.

9. **Re-verify** `tools/list`/`tools/call` end-to-end against the now-live
   worker route, and re-run step 7's fault-isolation check (stop the
   worker, confirm only its own tools go "unavailable", confirm every other
   domain — compiled-in or worker-routed — is unaffected).

10. **Ship a systemd unit** for the new worker process. Template (adapt
    paths/env vars per `deploy/review-daemon.service`'s conventions — no
    hardcoded infra values, secrets via an `EnvironmentFile` never
    committed):

    ```ini
    [Unit]
    Description=Terminus <domain> worker
    After=network.target

    [Service]
    Type=simple
    # All domain secrets (API tokens, DB URLs, etc.) are provisioned via
    # <secret-manager> at deploy time and materialized into this EnvironmentFile --
    # never hardcoded here or in source. See this repo's secrets-discipline
    # rules.
    EnvironmentFile=${TERMINUS_WORKER_ENV_FILE}
    Environment=TERMINUS_WORKER_SOCKET_PATH=${TERMINUS_WORKER_SOCKET_PATH}
    ExecStart=${TERMINUS_WORKER_BIN_PATH}
    Restart=on-failure
    RestartSec=5
    NoNewPrivileges=true
    PrivateTmp=true
    ProtectSystem=strict
    ProtectHome=read-only
    ReadWritePaths=${TERMINUS_WORKER_SOCKET_DIR}

    [Install]
    WantedBy=multi-user.target
    ```

**Never merge step 8 (removing `register_all`) until step 6's deployment
and registration is confirmed live.** Merging the `register_all` removal
first — before the worker is deployed and registered — makes the gateway
lose that domain's tools the moment the merged code is deployed, with no
fallback. The compiled-in code and the worker registration must overlap in
time; only the git history needs to be sequential.

## 7. Secret-isolation checklist per worker

Apply this checklist to every new `terminus-<domain>` worker before it goes
live, and re-apply it whenever a worker's tool set changes:

- [ ] The worker's process environment contains **only** the secrets its own
      domain's tools actually need — never a copy of the broker's full
      secret set "just in case".
- [ ] Every secret is resolved through `SecretManager::get()` /
      `vault::manager().get()` (per this repo's secrets-discipline rule),
      never a raw `std::env::var(...)` for anything token/key/password/
      secret-shaped, and never <secret-manager> fetched ad hoc outside that path.
- [ ] The worker's declared `capability_class` (the snake_case registration
      value) accurately reflects whether it holds secret material
      (`secret_holding`) — do not under-declare a secret-holding worker as
      `read_only`/`write_scoped` to dodge the T2 floor; the floor exists
      specifically to force strong transport security onto exactly this
      class.
- [ ] The worker's systemd unit uses an `EnvironmentFile` populated at
      deploy time (never a literal secret in the unit file, in source, or in
      a commit) and appropriate hardening (`NoNewPrivileges`, `PrivateTmp`,
      `ProtectSystem=strict`, `ProtectHome=read-only`, a minimal
      `ReadWritePaths`).
- [ ] The worker's UDS socket path (T1/T2) or listening port (T0) is scoped
      so only the broker process (and, for T1/T2, only via the correct
      uid/cert identity) can reach it — no world-writable socket, no
      unauthenticated TCP listener.
- [ ] If the domain is extracted from `register_personal` as well as
      `register_all`, confirm which consumer surface(s) (core/Chord vs.
      personal/`terminus_personal`) the worker is meant to serve, and scope
      its registration (and the admin identity used to register it)
      accordingly — do not silently expose a personal-only domain's worker
      to the core surface, or vice versa.
- [ ] After extraction, confirm (e.g. via `core_personal_name_collisions`-
      style reasoning, or a direct registry inspection) that no compiled-in
      module anywhere in the crate still references the extracted domain's
      removed types/functions — a leftover call site is a sign the
      extraction (§6 step 3) missed a real dependency.
