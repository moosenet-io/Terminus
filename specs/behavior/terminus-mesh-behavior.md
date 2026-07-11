# terminus-rs mesh — behavior spec

Spec: `MESH-14` (Plane project `TERM`, prefix `MESH`), closing out the MESH-01..12
feature set with a durable behavior contract + end-to-end verify.

Source: `src/mesh/{registry,client,merge,principal,onboarding,client_onboarding}.rs`,
`src/gateway_framework/mod.rs`, `src/approval.rs`, `src/mcp_server.rs` (federated
`tools/list`/`tools/call` dispatch).

Verify: `tests/mesh_e2e.rs` (`cargo test --test mesh_e2e`) drives every contract below
against two in-process mock upstreams (`httpmock::MockServer`, bound to loopback —
never a real infra host) plus a deliberately-unreachable third upstream for the
health-drop contract. No live server, Postgres, or network dependency is contacted by
that test.

## States

### State: Upstream — Healthy

- entry: `UpstreamPool::from_registry` builds the client successfully (valid
  transport config, resolvable secret for Bearer) and either has not yet been
  probed (optimistic default) or its last `/healthz` probe succeeded.
- exit: a `/healthz` probe fails → **Unhealthy** (backoff scheduled, see
  `UpstreamPool::health_check_all`).
- verify:
  - `port_listening("${MESH_TEST_UPSTREAM_HOST}", "${MESH_TEST_UPSTREAM_PORT}")` for a
    live deployment's configured upstream (the E2E test substitutes an httpmock
    loopback address; a real deployment resolves this env pair from the upstream's
    registered `url`, never a literal IP in this spec).
  - `UpstreamPool::healthy_clients()` includes the upstream's client.
  - `MergedCatalog::build` includes the upstream's tools, namespaced
    `<namespace>__<tool>`.

### State: Upstream — Unhealthy

- entry: a `/healthz` probe fails (timeout, connection refused, non-2xx), or the
  client could never be constructed at all (missing/blank secret, mTLS config
  failure — excluded from the pool entirely at `from_registry` time, logged via
  `tracing::warn!`, never fatal to sibling upstreams).
- exit: a subsequent `/healthz` probe (once its exponential backoff window,
  2s doubling to a 120s ceiling, has elapsed) succeeds → **Healthy**.
- verify:
  - `UpstreamPool::healthy_clients()` excludes the upstream's client.
  - `MergedCatalog::build` excludes that upstream's namespaced tools from the
    merged `tools/list`, while every OTHER (healthy) upstream's tools and all
    local tools are unaffected — one bad upstream never takes the others down.
  - `resolve_call_route("<namespace>__<tool>", pool)` returns
    `CallRoute::Unavailable { namespace }`, never falls back to local dispatch and
    never attempts the call.

## API Contracts

### API: `tools/list` (merged catalog)

- input: none (an MCP `tools/list` request, optionally scoped by the caller's
  resolved `Principal`).
- output: local core tools (and the pre-existing single personal-registry
  federation) unprefixed, plus every currently-**Healthy** mesh upstream's tools
  namespaced `<namespace>__<tool>` (`crate::mesh::merge::namespaced`,
  separator `__`, `MESH_NS_SEP`).
- verify:
  - two upstreams that each export the SAME bare tool name (e.g. both export
    `widget_status`) never collide in the merged list: the local (if any),
    upstream-A-namespaced, and upstream-B-namespaced forms are three distinct
    entries.
  - `split_namespaced(namespaced(ns, tool)) == Some((ns, tool))` round-trips for
    every advertised namespaced name.
  - `AllowlistPolicy::filter_tools(identity, tools)` (RBAC) narrows this list
    per-identity BEFORE it is ever returned to a caller — see "RBAC deny
    contract" below. This is the single source of truth for both `tools/list`
    visibility and `tools/call` enforcement (same `Grant::permits` decision,
    same advertised name).
- error_cases:
  - an upstream's `list_tools()` call fails mid-build (transport error, bad
    response shape) → that upstream is excluded from this build's catalog only
    (`tracing::warn!`, not a build failure); the merge still returns a valid
    catalog for every other upstream + local tools.

### API: `tools/call` (federated routing)

- input: an advertised name (bare local, or `<namespace>__<bare_tool>`) + arguments.
- output: routed via `resolve_call_route` (a cheap, zero-network-I/O lookup
  against the pool's CURRENT health state — deliberately not a cached
  `RoutingTable` from the last `tools/list`, which could be stale the instant an
  upstream's health flips):
  - `CallRoute::Local` — genuinely local name, OR a `__`-shaped name whose prefix
    is not a currently-known mesh namespace at all (never coincidentally treated
    as a namespaced call).
  - `CallRoute::Upstream { client, bare_name }` — the namespace IS a currently
    Healthy upstream; dispatch `bare_name` to `client.call_tool(...)`.
  - `CallRoute::Unavailable { namespace }` — the namespace IS a known, registered
    upstream, but it is not currently Healthy (down, or excluded from the pool
    at startup) → a clean tool-error (`upstream_unavailable_text`), never a
    silent fallback to local dispatch.
- verify:
  - a `tools/call` to `<namespaces-A>__widget_status` reaches upstream A's mock
    and returns upstream A's distinct response text; the same call against
    `<namespace-B>__widget_status` reaches upstream B's mock and returns B's
    distinct response text — proving actual routing, not just name construction.
  - HTTP 200 with a JSON-RPC `"error"` object inside is a TOOL-level failure
    (`UpstreamCallResult { is_error: true, .. }`), never a transport `Err` —
    federation must not conflate "the upstream ran the tool and it failed" with
    "the upstream could not be reached at all".
- error_cases:
  - unreachable/timed-out upstream → `UpstreamClientError::Unreachable` /
    `::Timeout`, surfaced cleanly, never a panic.
  - namespace known but currently unhealthy → `CallRoute::Unavailable`, no call
    attempted at all.

### API: RBAC deny contract (`AllowlistPolicy`)

- input: a resolved caller identity (a `Principal::name()`, or any bare identity
  string for legacy/service callers) + an advertised tool name (bare or mesh
  namespaced).
- output: `AllowlistPolicy::is_allowed(identity, name)` — default-deny: an
  identity with NO entry in the policy at all is denied every action, not just
  unmapped ones. `AllowlistPolicy::filter_tools(identity, tools)` applies the
  same per-tool decision across a whole catalog.
- verify:
  - an identity absent from the policy map (`has_any_entry == false`) gets an
    EMPTY filtered catalog from `filter_tools`, and `is_allowed` returns `false`
    for every tool, including namespaced mesh tools.
  - a mapped identity granted only one upstream's namespace (e.g.
    `"<namespace-A>__*"`) sees and may call that namespace's tools but NOT the
    other upstream's — a grant never leaks across mesh namespaces.
  - a `Grant::AllowDeny` deny-prefix wins even over an `allow: ["*"]` wildcard,
    and matches BOTH the namespaced form and the bare (post-`__`) tool name, so
    a sensitive local deny prefix (e.g. `github_`) still closes the same tool
    re-exported through any upstream namespace.

### API: Approval-gate propagation contract (federated guarded tools)

- input: a `tools/call` whose resolved bare name is in `approval::GUARDED_BARE_NAMES`
  (the same static classification local dispatch already gates on — <secret-manager>/
  ansible/openhands/routines/git-mirror tools), routed to a mesh upstream.
- output: `src/mcp_server.rs`'s federated dispatch path runs
  `approval::gate(bare_name, approval::mesh_gate_args(args, upstream_namespace),
  summary)` BEFORE the call ever leaves this process — federation must never be a
  way to bypass human approval, and this gate is authoritative regardless of
  whatever gate the upstream itself may also enforce (double-gating is fine,
  never skipped).
- verify:
  - `approval::is_guarded(bare_name)` classifies a federated call identically to
    a local one (same static list, no drift).
  - `approval::mesh_gate_args(args, "<namespace-A>")` and
    `approval::mesh_gate_args(args, "<namespace-B>")` produce DIFFERENT bound
    content for the same real `args` — a code approved for one upstream's call
    can never be replayed against another upstream's (or a local) same-named
    call (content-binding, MESH-09).
  - with no reachable approval-grant store (`DATABASE_URL` unset — this repo's
    established "no live Postgres in a hermetic test" posture, see
    `src/approval.rs`'s own MESH-09 tests), `approval::gate` returns
    `Gate::Denied` cleanly — never panics, never hangs, never silently grants.
    A full Granted/Pending/consumed-once redemption against a live Postgres is
    OUT OF SCOPE for this hermetic contract; that requires a real database and
    is exercised by `src/approval.rs`'s own DB-backed test suite, not this E2E.

## Data Contract: onboarding (MESH-11/12)

### Data: candidate-upstream onboarding report (`OnboardingReport`)

- format: an in-memory struct (never persisted to disk by this item), returned by
  `onboard_upstream` — probes the candidate, discovers its catalog, checks
  namespace/name collisions against the current registry, confirms trust
  readiness, previews the merge delta. Never mutates live config; the operator
  hand-edits `TERMINUS_MESH_UPSTREAMS_JSON` afterward.
- required_fields: candidate reachability, discovered tool count, namespace
  collision status, trust status (`TrustStatus`).
- verify:
  - `json_valid` when serialized for the `mesh_onboard_upstream` CORE tool's
    response.
  - never includes a resolved secret VALUE (only the `secret_key` NAME, if any)
    — mirrors `UpstreamServer`'s own "Debug never leaks a secret" invariant.

### Data: candidate-client onboarding report (`OnboardClientReport`)

- format: an in-memory struct returned by `onboard_client` — mints/records the
  client's identity (embedded-CA cert or tailnet mapping), maps it to a
  canonical `Principal` name, seeds a least-privilege
  `AllowlistPolicy` grant (`LEAST_PRIVILEGE_CLIENT_GRANT_TOOLS`, never
  default-allow `"*"`), emits a ready-to-use connection profile.
- verify:
  - the seeded grant is never `Grant::List(vec!["*".to_string()])` and never an
    `AllowDeny` with an unrestricted `allow: ["*"]` and empty `deny` — onboarding
    a new client must start least-privilege, an operator broadens it later, not
    the other way around.
  - the emitted profile is config for the operator to persist, never applied to
    live policy directly by this call.

## Deferred / not covered by this item

- A live-Postgres approval Granted→consumed redemption cycle (covered by
  `src/approval.rs`'s own DB-backed tests, not this hermetic E2E).
- mTLS-transport upstream dial correctness beyond "client builds against the
  embedded CA and fails cleanly when the peer is dead" (covered by
  `src/mesh/client.rs`'s own `mtls_upstream_client_builds_against_embedded_ca_and_fails_cleanly_when_dead`
  test; this item's E2E uses Bearer transport for its two live mock upstreams
  since that is what httpmock's plain-HTTP mock server can actually terminate).
- Tailnet WhoIs resolution (`crate::mesh::tailnet`, `tsnet` feature-gated,
  off by default) and the `MESH-06` `PrincipalResolver` cert-CN/tailnet
  reconciliation path itself — this item exercises the RBAC DECISION
  (`AllowlistPolicy`) once an identity string is already resolved, not the
  resolution step that produces one.
