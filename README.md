<h1 align="center">Terminus</h1>

<p align="center"><em>The Lumina Constellation's MCP tool hub: 381 core fleet tools behind one authenticated gateway, with model-intake, code-knowledge, review, and CI/CD engines built in.</em></p>

<p align="center">Rust · 410 modules · 381 core MCP tools (+3 personal-only) · 11,905 KG nodes / 27,107 edges · analyzed <code>3d0f277</code></p>

<p align="center"><a href="docs/index.md">Docs</a> · <a href="docs/getting-started.md">Getting Started</a> · <a href="docs/reference/index.md">Reference</a> · <a href="docs/architecture.md">Architecture</a> · <a href="docs/guides/index.md">Guides</a></p>

---

## What is Terminus

Terminus (crate `terminus-rs`) is the tool plane of the Lumina Constellation — a fleet of
self-hosted AI-agent services. Every operational capability an agent may invoke is
implemented here as a typed Rust `RustTool` and dispatched through one
`ToolRegistry` (`src/registry.rs`): project tracking (`plane_*`, 43 tools), source
forges (`gitea_*`, `github_*`, provider-agnostic `git_private`/`git_public`), the
sanctioned Postgres door (`pg_*`), model intake and profiling (`model_intake*`),
code review (`review_run`), the Atlas code knowledge graph (`kg_*`), the
documentation engine (`docgen_*`, `scribe_*`), the constellation CI/CD build door
(`compiler_*`), media orchestration, and a long tail of fleet utilities. Tools never
shell out; the `RustTool` contract restricts them to typed HTTP and parameterized
SQL, and the few capabilities that genuinely need subprocesses (CLI review
providers, docgen inspection) live behind dedicated loopback daemons.

Two deployments serve two registries from the same crate. `terminus_primary` is the
gateway: it registers the core tool set (`register_all`, 381 tools), fronts it with
mTLS + enrollment (`pki`, `terminus-client`), resolves every caller to a unified
`Principal` identity (`mesh::principal`), and federates outward — to personal-registry
tools via the Chord relay (`federation`), to other Terminus-shaped upstream servers
(`mesh`), and to out-of-process tool workers (`broker`, `terminus-worker-sdk`).
`terminus_personal` is the second deployment: the operator's personal/admin subset
(`register_personal`, 189 tools) served over plain streamable-HTTP MCP, with
downstream secrets fetched fresh from the vault at startup.

Around the hub sit engines that make the fleet self-maintaining: **intake** (the
largest subsystem, 2,059 symbols) profiles every candidate model — context, coder,
and assistant suites — and stores operational profiles; **scribe/Atlas** builds a
per-project code knowledge graph that grounds review prompts, blast-radius queries,
and this documentation; **cortex** scores structural elegance and change risk from
that graph; **forge** maintains PII-swept public mirrors of internal repos; and
**compiler** is the single sccache-backed build door for the constellation's CI/CD.

## Architecture

Derived from the code knowledge graph's cross-subsystem call edges (node label =
subsystem · symbol count; see [docs/architecture.md](docs/architecture.md) for the full version):

```mermaid
flowchart LR
    BIN["bin · 271<br/>12 binaries"] --> MESH["mesh · 258<br/>principal + federation"]
    BIN --> MCP["mcp_server + registry<br/>tool dispatch"]
    MESH --> MCP
    MCP --> INTAKE["intake · 2059"]
    MCP --> FORGE["forge · 836"]
    MCP --> TOOLS["tools/docgen · 772"]
    MCP --> PLANE["plane · 514"]
    MCP --> PG["pg · 219"]
    MCP --> BROKER["broker · 213<br/>worker fall-through"]
    TOOLS --> SCRIBE["scribe / Atlas KG · 739"]
    REVIEW["review · 263"] --> SCRIBE
    CORTEX["cortex · 424"] --> SCRIBE
    MCP --> REVIEW
    MCP --> CORTEX
    FORGE --> GH["github (PII gate) · 255"]
```

## Subsystems

| Subsystem | What it does | Reference |
|---|---|---|
| `intake` | Model discovery, profiling suites (context/coder/assistant), GPU authority, fleet assessment | [reference/intake](docs/reference/intake.md) |
| `forge` | Provider-agnostic git domains (`git_private`/`git_public`), adapters, PII-swept public mirror engine | [reference/forge](docs/reference/forge.md) |
| `tools` | The docgen documentation engine and serving control/status tools | [reference/tools](docs/reference/tools.md) |
| `scribe` | Atlas per-project code knowledge graph + standing documentation agent (`kg_*`, `scribe_*`) | [reference/scribe](docs/reference/scribe.md) |
| `plane` | 43 Plane CE project-tracking tools: multi-identity PATs, shared Redis cache + rate budget | [reference/plane](docs/reference/plane.md) |
| `cortex` | Atlas-backed blast-radius, elegance metrics, risk scoring, calibration | [reference/cortex](docs/reference/cortex.md) |
| `media` | Typed clients for the self-hosted media stack and request/search/recommend tools | [reference/media](docs/reference/media.md) |
| `gitea` | 20 Gitea REST tools with named-identity PATs and the merge queue | [reference/gitea](docs/reference/gitea.md) |
| `review` | `review_run`: multi-provider, multi-structure code review with KG-grounded prompts | [reference/review](docs/reference/review.md) |
| `github` | GitHub org tools and the authoritative PII scan/redact engine | [reference/github](docs/reference/github.md) |
| `mesh` | Upstream Terminus federation registry, unified `Principal` identity, optional embedded tailnet | [reference/mesh](docs/reference/mesh.md) |
| `broker` | Out-of-process tool workers: route table, three transport tiers, blue-green rollout | [reference/broker](docs/reference/broker.md) |
| `pg` | The single sanctioned Postgres door: identity-scoped, approval-gated `pg_*` suite | [reference/pg](docs/reference/pg.md) |
| `agent_router` | The agentic tool router: identity-scoped selection, local dispatch, Chord for inference only | see below |
| `availability` | Tool availability state (`available`/`off`/`broken`) — park a dead tool without de-registering it; `tool_availability` is the admin view | see below |
| `oauth` | The OAuth 2.1 remote-MCP connector door: authorization server, login/consent, the token endpoint that mints audience-bound access tokens, and per-client tool and server scoping | see below |

### Agent tool router — selection, dispatch, and caching

`POST /v1/agent/execute` runs the assistant's tool turn **in Terminus**. It previously
blind-forwarded to Chord, which ran the loop against its own catalog — but Chord has no
caller identity, so tool exposure could not be scoped per user, and its catalog pointed
at a stale backend. The loop now runs where the principal is already resolved and
authorization already lives; Chord is called only for the tool-selecting sub-agent's
inference, via a **named proxy** (never a hard-wired model).

Per turn:

1. **Select** — three filters compose, each of which can only *remove*:
   authorization (per-principal allowlist) ∩ availability (is the tool alive at all) ∩
   relevance (lexical match on the request). Authorization and availability run
   **first**, so an unauthorized or parked tool is never even a candidate — the model
   cannot be tempted into calling something it would then be refused.
2. **Ask Chord** for a completion, offering only those tools.
3. **Dispatch locally** through the registry, behind the result cache.
4. Repeat until the model answers, or the bounds below are hit.

Bounds: 8 tool calls and a 90 s wall clock, deliberately **under** the client's egress
timeout so the router's own message surfaces rather than a dead socket. A turn that
hits either bound still returns the data it did fetch instead of an error.

| Variable | Default | Purpose |
|---|---|---|
| `TERMINUS_ROUTER_LOCAL` | on | `0` restores the previous blind-forward to Chord — rollback with no redeploy |
| `TERMINUS_ROUTER_MODEL` | `lumina-fast` | Chord **named proxy** the selection sub-agent runs on |

Streaming callers (`"stream": true`) receive the same SSE progress-event frames Chord
emitted — `tool_call_started`, `tool_call_complete`, `complete` — so existing clients
need no change. A `complete` frame is **always** emitted, including on timeout, because
a client that never sees one waits forever.

### Tool result caching — the common path stays fast

The assistant's highest-traffic tools (news, weather) are cached so a conversational
question does not pay a live upstream round-trip every time.

- **Opt-in per tool.** Anything without a policy is never cached; behaviour is unchanged.
- **Stale-while-revalidate.** Past the soft TTL the cached value is returned
  *immediately* and refreshed in the background — you wait on a slow upstream at most
  once. Only one caller refreshes; the rest are served from cache.
- **Seeded policy** — `news_*`: 15 min soft / 24 h hard (a daily pull that still moves
  through the day). `weather`: 20 min soft / 6 h hard. The hard bound means stale data
  can never be presented as current.
- **Severe-weather alerts are never cached.** Freshness beats latency where safety is
  involved; a stale all-clear is worse than a slow answer.
- **Live-state reads are never cached either** — `media_now_playing` is named in an
  exact-name never-cache list, so it survives any future `media_*` prefix policy. A
  cached "what is playing right now" is not stale data, it is a false statement — and a
  cache hit would also bypass that tool's per-caller entitlement gate.
- **Errors are never cached as data** — only a short failure backoff, and a failed
  background refresh leaves the last-good value intact rather than poisoning it.
- **Results carry `fetched_at`** so the assistant can say "as of …" instead of implying
  cached data is live.
- **User-scoped results are keyed by principal** and never shared between users.
- Bounded with oldest-first eviction.

### Tool grants — who may call what (and who is a guest)

Every request is gated by a **per-identity grant map**: a principal (mTLS CN,
tailnet identity, or named PAT) is looked up in `TERMINUS_GATEWAY_ALLOWLIST_JSON`
and its grant decides whether the action — a tool name, an inference route, or an
`admin:` op — is permitted. **Default-deny**: an identity with no entry is denied
everything. The same decision drives `tools/list` visibility and the router's
selection step, so a tool you cannot call is a tool you are never shown.

Two grant shapes: a plain allow list (`["ledger_accounts", "*"]`) and an
allow/deny object (`{"allow": ["*"], "deny": ["github_", "infisical_"]}`), where
deny entries are literal **prefixes** that win even over an allow `"*"`.

> #### ⚠ Before you provision a guest: what the guest baseline does **not** protect (TERM #577)
>
> The baseline constrains a caller that authenticates as its **own gateway
> principal** — its own client cert / tailnet identity / named PAT, with its own
> entry in this map. It does **not** yet distinguish two humans sharing one
> identity, and today they do: **every person who talks to Lumina arrives at the
> gateway as `identity=lumina`.** The mTLS principal names the *service*, not the
> person; the human identity known at the web edge (`X-Lumina-User`,
> `src/constellation/proxy.rs`) is not forwarded through Chord and never reaches
> `gateway_framework`.
>
> So a houseguest conversing with the assistant today is authorized as `lumina`
> — holding `google_calendar_today`, `commute_estimate` and full inference — and
> **none of the guest narrowing below applies to them.** The same limit applies
> to the per-principal tool cache: it isolates `lumina` from a guest principal,
> not two humans who are both `lumina`.
>
> **Provisioning `guest-*` principals without closing TERM #577 gives a FALSE
> sense of containment.** The guest surface is real only for a **separately
> authenticated principal**. Closing the gap needs end-to-end human-identity
> propagation — design work tracked as **TERM #577**, a blocker for the `hearth`
> family sprint — not a wider or cleverer grant map.

**Guest / family identities** get a deliberately different construction. Name
them in `TERMINUS_GATEWAY_GUEST_IDENTITIES` (comma-separated) and each gets the
baseline surface — ten entries: the assistant route `/v1/agent/execute` (but not
raw completions), plus **nine tools** (`time_now`, `weather`, the three `news_*`
tools, and the four media *discovery* tools) — as an **exactly enumerated allowlist**, not a wildcard
minus a denylist. That is the load-bearing choice: with a denylist every tool
family added in future would be granted to houseguests the day it registers.
With the allowlist a new family is invisible to a guest until someone
deliberately adds it. Media acquisition/mutation (`media_request`,
`media_delete`, `media_organize`, `media_taste_feedback`) is excluded for the
same reason the entries are exact names rather than a `media_*` prefix.

**Media personalisation is scoped to the caller** (TERM #576). Two of the
discovery tools reach household watch history, so a grant of the tool is not on
its own enough: `media_recommend` builds its taste profile from the account the
gateway resolved for the *caller*, and `media_on_deck` discloses the
continue-watching row only to the account it belongs to. Bind a principal to a
household media account with `TERMINUS_MEDIA_ACCOUNT_MAP` (a JSON object of
principal → media account id) and name the account your `PLEX_TOKEN` speaks for
in `PLEX_ACCOUNT_ID`. Both default to **unset = nobody**: an unmapped caller
gets an unpersonalised library browse with no titles, rationales or curation
notes drawn from anyone's history, and a caller-supplied `account_id` naming
someone else's account is **refused, not silently ignored**. The same TERM #577
scope limit applies — this constrains separately authenticated principals, not
two humans sharing one assistant identity.

| Variable | Default | Purpose |
|---|---|---|
| `TERMINUS_MEDIA_ACCOUNT_MAP` | unset (nobody mapped) | `{"<principal>":"<media account id>"}` — whose media account each principal is |
| `PLEX_ACCOUNT_ID` | unset (withheld from all) | The media account `PLEX_TOKEN` speaks for; gates `media_on_deck` |

That baseline is a **ceiling, not a default**. For every other identity an
explicit `TERMINUS_GATEWAY_ALLOWLIST_JSON` entry wins in full; for a guest it is
*intersected* with the baseline, so an entry may **narrow** a guest and can never
widen one. A `{"guest-alex": ["*"]}` — one wildcard, or a line copy-pasted from
an operator identity — would otherwise have handed a houseguest
`google_calendar_today`/`commute_estimate`, which is what the gateway reads to
decide whether a tool may fold the operator's calendar or home address into an
answer. A clamp is logged loudly; to grant more than the baseline, take the
identity out of `TERMINUS_GATEWAY_GUEST_IDENTITIES`.

Grants are **validated fail-closed** at load: a malformed entry (wrong JSON
type, an unknown key such as a misspelled `deny`, a whitespace/empty entry, a
`*` in a deny prefix where it would silently match nothing) is **dropped and
that identity denied**, never coerced into something broader — and one bad entry
never discards the rest of the map.

Full model, the guest surface with per-tool rationale, and a worked example for
adding a principal: [docs/reference/tool-grants.md](docs/reference/tool-grants.md).

| Variable | Default | Purpose |
|---|---|---|
| `TERMINUS_GATEWAY_ALLOWLIST_JSON` | `{}` (deny all) | The grant map: `identity -> grant` |
| `TERMINUS_GATEWAY_GUEST_IDENTITIES` | unset | Comma-separated identities that get the guest/family baseline |

#### Telling two humans apart behind one service identity (TERM #595)

Every warning above about TERM #577 has the same root: a gateway `Principal`
names a **service**, and every person who talks to the assistant arrives as
`lumina`. TERM #595 is the mechanism that lets a turn say *which human it is
for*, in a way no hop in between can FORGE. That is the precise claim, and it is
narrower than it first sounds: a hop cannot mint a valid assertion (it has no
signing key) and cannot replay one under a different principal (each is bound to
the principal it was minted for). It is *not* proof against a hop that replaces
the header with another captured, still-valid assertion for the same principal —
see the replay limits in `src/mesh/person.rs`.

A trusted front door — one that is mutually authenticated (mTLS/tailnet) **and**
holds the `admin:assert_person` grant — sets `X-Terminus-On-Behalf-Of: <person>`
on its request. Terminus translates that, at that door only, into a **signed,
short-lived assertion bound to the asserting principal**, which downstream hops
relay but cannot forge (they hold no signing key). At the tool-dispatch door the
assertion is verified, re-bound to the principal on *that* hop, and turned into
the caller's person scope.

What the person scope changes today:

* **Media personalisation** resolves from the *person* (`person:<id>` in
  `TERMINUS_MEDIA_ACCOUNT_MAP`) with **no fallback** to the principal's account.
* **Calendar/routine entitlements** become the *intersection* of the service's
  grants and that person's own `person:<id>` grant — a person can only ever
  **narrow** what the service could already do, never widen it.

Fail-closed, in the one direction that matters — and the line falls between an
identity that was never **claimed** and one that was claimed and cannot be
**honoured**:

* **No identity headers at all** → the unchanged, service-scoped path every
  pre-#595 caller already took. This is the baseline, not a downgrade.
* **Claimed but blank, malformed, expired, mis-bound, asserted by a principal
  without the grant, or presented on a hop that cannot honour it** → **less**
  privilege than the bare service identity, never a silent fallback to it.

An `X-Terminus-On-Behalf-Of` request that cannot be honoured is **refused with
`403`**, because quietly running it as the service would be a *widening*: the
caller believes it is acting as one person while actually reading and writing
the **shared** record. For the same reason, a client-supplied
`X-Terminus-Person-Assertion` is refused rather than stripped — these headers are
server-set on every hop, so an inbound copy is never authoritative.
A client-supplied copy of either header is stripped on every relay hop; the only
value that ever reaches the next hop is the one the server set.

Both settings default to unset, and an unconfigured deployment behaves exactly
as it did before — the roster is a **closed list**, so an unlisted or misspelled
person is refused rather than quietly given a fresh empty identity.

| Variable | Default | Purpose |
|---|---|---|
| `TERMINUS_PERSON_IDENTITIES` | unset (nobody assertable) | Comma-separated household person identifiers |
| `TERMINUS_PERSON_ASSERTION_KEY` | unset (mechanism inert **and closed**) | HS256 key for signing/verifying person assertions; materialized from the secret store |

Grant a front door the right to speak for people with an **admin-namespaced**
entry — a tool wildcard (`"*"`) deliberately does **not** confer it:

```json
{"lumina": {"allow": ["*", "admin:assert_person"]},
 "person:alex": ["weather", "media_recommend"]}
```

**Scope, stated plainly:** this ships the trusted transport, its authorization,
and the threading into the caller context. Lumina does not yet *send*
`X-Terminus-On-Behalf-Of`. The proxy ingress DOES already mint the signed
assertion and emit it on the upstream request to Chord; what is missing is Chord
propagating it onward to MCP tool dispatch, so end-to-end propagation is not live
until that and Lumina's side land. Until then every turn is
still service-scoped — the same posture as before, not a weaker one.

### Severe-weather watch — acting before it matters

`weather_severe_alerts` answers *"is something coming that I need to act on?"*, as
opposed to `weather`, which answers *"what's it like in X?"*. It watches exactly two
things, both chosen because the operator actually acts on them:

- **Storms that disrupt travel.** It reads real upcoming trips from the calendar and
  checks the weather **at the destination on the travel day**, so a warning is about a
  flight you actually have — not a generic "storms this week". Flight-shaped events are
  called out separately, because there the useful action is *rebook now*.
- **Heat waves as a home power / HVAC risk.** The trigger is *sustained* heat at the
  home location: consecutive days at or above **32.2 °C / 90 °F** whose nights stay at
  or above **21.1 °C / 70 °F**. The warm-night half is the point — a hot afternoon
  followed by a cool night is not a power problem, because the building sheds its heat
  and the AC rests. Heat becomes a *supply* problem when the nights stop helping.

Travel thresholds, likewise deliberately conservative so the watch does not cry wolf:
gale-force wind ≥ **17.2 m/s** (Beaufort 8), heavy rain ≥ **10 mm/3h**, any meaningful
snow ≥ **2.5 mm/3h** water-equivalent, precipitation at or below freezing (icing), plus
the thunderstorm, squall, freezing-rain and tornado condition groups. Light rain and a
breeze are not disruption, and a plain warm day is not a heat wave.

Three properties worth knowing:

- **It is a pull-mode tool, not a push.** Terminus has no proactive delivery path to a
  human — it holds no Matrix connection, and its only timed feature (`reminder`) is
  deliberately built as poll-from-outside, with lumina-core owning delivery. So this
  ships the assessment; scheduling and delivery belong where they already live.
- **Derived, not official, by default.** The `/data/2.5/*` endpoints return no `alerts`
  array; government alerts come only from One Call 3.0, a separate subscription. Set
  `OPENWEATHER_ONECALL_ALERTS=1` to fetch them, in which case official alerts are
  reported first and labelled as such. Otherwise every finding says it is derived.
- **It degrades honestly.** *"Checked, nothing severe"* and *"could not check"* are
  different answers and are worded differently — no calendar, no configured home
  location, or a provider outage never renders as an all-clear. It never invents a
  location.

Reading the calendar and the home location is gated on the same per-source entitlement
as location inference (`google_calendar_today` / `commute_estimate`), and the two are
gated **independently**. An unentitled caller gets nothing *and causes no read of either
source* — a guest cannot learn that the operator is travelling, where to, or where they
live. Results are **never cached**; a stale all-clear is the one failure mode this tool
must not have.

### Saved locations — one registry, many consumers

`location_set`, `location_list` and `location_clear` let the assistant remember
places **conversationally** — *"I've moved"*, *"remember this is home"*, *"I'm in
Denver this week"* — instead of someone editing config on a host. What they write
is a **shared registry**, not a weather feature: `weather` was the first
consumer wired to it and `commute_estimate` / `route_traffic` /
`traffic_incidents` are the second, reading the same registry through the same
contract (`crate::locations`) — which is what wiring commute needed: the same
call, no registry change. News and future modules read it the same way. Adding a consumer needs no change
to the registry.

Entries are named. `home`, `work` and `current` are the well-known names other
tools understand; anything else the user chooses ("the cabin") is stored and
retrievable by name. An entry is permanent, or **temporary with an absolute
expiry** — which is what makes a travel override safe: `current` outranks
`home`/`work` while it lasts, and because it expires it cannot quietly become
where you live.

Four properties worth knowing:

- **Per authenticated principal — NOT per person today.** Records are keyed on a
  caller identity derived from the server-verified principal, and reading or
  writing needs the same entitlement that governs home/work location inference
  (`commute_estimate`). Two separately-authenticated *principals* cannot read or
  write each other's records, and an unentitled caller causes **zero reads** —
  not a read whose result is discarded. But read the gap plainly rather than the
  optimistic version: **every human talking to Lumina currently arrives as one
  service identity (TERM #577), so everyone behind it shares a single record and
  sees the same saved home.** This is per-*principal* isolation, and nothing
  here should be read as per-*person* isolation until #577 closes — at which
  point it becomes genuinely per-person with no rewrite, because the key already
  has room for a person. Records written before then are **orphaned rather than
  shared**: a re-entry prompt is cheap, a silently shared home address is not.
- **Absence and failure stay distinct.** *"You have nothing saved"* and *"I
  couldn't read what you've saved"* are different answers and are worded
  differently, everywhere. A location is **never invented or inferred** to fill
  either gap.
- **Writes are deliberate.** Replacing an existing different value needs an
  explicit confirmation; clearing everything needs `all=true`. `weather`'s
  "or say *remember this is home*" is an **offer**, not an automatic save —
  answering a question is not consent to store the answer.
- **`COMMUTE_HOME` / `COMMUTE_WORK` / `COMMUTE_FAMILY` are no longer read at
  all — by weather *or* by commute.** Weather stopped first; commute kept
  reading all three directly out of the process environment until **TERM #591**,
  which is the same disclosure in the module the variables are named after, and
  removing it from one consumer while leaving it in the other closed half a
  hole. In weather they were also kept
  briefly as a migration fallback, scoped to one service principal. That does
  not work and cannot be made to: until **TERM #577** lands, every human reaches
  Lumina as the *same* service principal, so "the configured principal" names
  the service they all share rather than the operator — and the fallback would
  hand the operator's home and work addresses to anyone entitled. Narrowing the
  gate does not help, because any gate keyed on a shared principal has the same
  defect. So the fallback is **deleted**, along with
  `TERMINUS_COMMUTE_LEGACY_PRINCIPAL`. All three variables are unset on the live
  host, so nothing working was lost; what was removed is a latent disclosure
  that would have activated the moment someone set them.

  The registry is the replacement, and the degradation is honest: with no saved
  home, `weather` asks *"which location do you mean?"* and
  `weather_severe_alerts` reports *"no home location is configured, so there is
  nowhere to watch"*. Both sit under the watch's standing *"this is unknown, not
  clear"* framing — an unwatched home is never an all-clear — but the **reason**
  stays distinct from *"your saved locations could not be read"*. Absence and
  failure are different answers, and neither is ever filled in by a guess.

  Commute degrades the same way. An omitted `origin` still **means** "home" —
  that is the tool's contract and the user's own saved place — but "home" is now
  the caller's registry entry, and with none saved the tool **asks** for a
  starting point rather than routing from somewhere else. *"Nothing saved under
  that name"*, *"saved locations aren't available on this connection"* and *"I
  couldn't read your saved locations"* are three different answers with three
  different wordings, and the last carries a different error type from the
  first — an absent value and a broken read are not the same class of problem.
  A literal address still routes for any caller, entitled or not: that discloses
  nothing.
- **Identities are opaque.** A caller key stores the authenticated principal (and,
  post-#577, person) **verbatim**, trimmed but never case-folded. `Alpha` and
  `alpha` are two callers with two records. Deciding that two differently-spelled
  identities are the same person is an authentication decision, not a storage
  one; if the principal namespace is ever specified as case-insensitive, the
  normalisation belongs upstream in the principal implementation.

| Variable | Default | Purpose |
|---|---|---|
| `TERMINUS_LOCATION_REGISTRY_PATH` | `~/.terminus/locations.json` | Where the registry document lives (owner-readable only, written atomically) |

### Tool availability — parking a tool without removing it

A tool whose backend has been retired should stop being offered to agents, but it should
still be **visible in the registry** so an operator can see what exists and why it is
parked. Deleting it loses that history; leaving it enabled makes the assistant try a dead
tool and report confusing failures.

Set `TERMINUS_TOOL_AVAILABILITY_JSON` to a map of tool name (or name **prefix**) to state:

```jsonc
{
  "crucible_": { "state": "off",    "reason": "retired 2026-07-30" },
  "odyssey_":  { "state": "off",    "reason": "retired 2026-07-30" },
  "hearth_shopping_list": { "state": "broken", "reason": "backend fault" }
}
```

- **States** — `available` (default), `off` (deliberately parked), `broken` (known-failing).
  `off` and `broken` behave identically to an agent; the distinction is for the human.
- **Matching** — an exact tool name wins over a prefix; among prefixes the longest wins, so
  a family-wide `"crucible_": off` can still be overridden by `"crucible_status": available`.
- **Effect** — a non-`available` tool is hidden from `tools/list` and **refused at call
  time** (so a stale cached catalog cannot invoke it). The refusal names the state and the
  reason rather than "not found", so the model parks it instead of hunting for it.
- **Fail-closed** — an unrecognised state resolves to `off`, never `available`: a typo must
  never silently re-expose a dead tool. A map that fails to parse parks **everything** and the
  service refuses to start. A variable that is **set but blank** is treated as a configuration
  error (almost always a failed template substitution such as `VAR=$MISSING`) and also fails
  closed — to disable availability rules, **unset** the variable rather than blanking it. Only
  a genuinely **unset** variable means "no rules, everything available" (unchanged behaviour).
- **Composes with authorization** — availability is principal-independent and can only
  *remove*. The per-identity gateway allowlist still applies independently; a tool is offered
  only if both allow it.
- **Admin view** — `tool_availability` lists every registered tool with its state and reason
  (optionally filtered by `prefix` or `state`).

Changing availability takes effect on service restart, like every other `Environment=` knob.

### Agent sessions — seeing the coder CLIs at work

Harmony can see a tracked repo's Plane status but nothing about the coder CLI agents
(Claude Code, codex, aider) actually working that repo. The `agentsess_*` suite is the
missing primitive: it enumerates live agent sessions on a host and correlates each to the
repository, branch, and `PREFIX-NN` work item it is working on.

- **`agentsess_list`** — live sessions with agent kind, pid, host, cwd, repo/branch, the
  work-item hint parsed from the branch, the tmux pane the session can be watched through,
  and its most recent activity time. Optional `host` (`local`, the default, or `dev` via the
  existing dev SSH door) and `repo` name filter.

Discovery is **process-first, not tmux-first**. Agents are not reliably launched
one-per-named-tmux-session — a host may run several inside one pane, or none in tmux at all
— so a tmux-driven enumerator would observe almost nothing. tmux is treated as an optional
*attachment* (matched by pane pid or process ancestry), never as the unit of discovery.

Each probe degrades independently: a host with no tmux, no readable transcript root, or no
git still returns a useful list, with the shortfall named in `warnings`. Truncation at
`AGENTSESS_MAX_SESSIONS` is likewise always reported — a silent cap would read as "that is
all of them", which is exactly wrong for an observability tool.

Session↔transcript matching is **exact** where it can be: Claude Code exports its session
UUID into its own environment, so it is read directly rather than guessed. Only that single
variable is ever extracted — a process environment routinely holds credentials, and nothing
else from it is read, returned, or logged.

| Variable | Default | Meaning |
|---|---|---|
| `AGENTSESS_TRANSCRIPT_ROOT` | `$HOME/.claude/projects` (local only) | Where agent transcripts live. **Required** to observe a remote host — assuming the local `HOME` applies remotely would silently probe the wrong path and report "no activity" as if it were a fact. |
| `AGENTSESS_AGENT_PATTERNS` | *(empty)* | Comma-separated extra program names to treat as agents, for a CLI this build predates. |
| `AGENTSESS_MAX_SESSIONS` | `50` | Result cap. Truncation is always reported, never silent. |

- **`agentsess_transcript`** — recent activity for one session: a summarised, redacted stream
  of what it has been *doing* (tool calls with their primary argument, messages), read from the
  tail of the session's transcript. Takes a `session_id` from `agentsess_list`, or an explicit
  `transcript_path` (jailed to the transcript root), plus `limit` and `host`.

Activity is **summarised, not streamed raw**: a transcript record carries a whole message, a
whole tool input, or a whole command's stdout, so each collapses to one short line. Only the
last `AGENTSESS_TAIL_BYTES` are read — transcripts reach tens of megabytes and none is ever read
whole. A line that is not JSON is skipped and counted into `skipped_lines`; a record that IS JSON but
whose shape is unrecognised is skipped and counted into `unknown_records`. The two are kept
apart because they have different causes — a truncated write versus a CLI format change — and
either being non-zero turns a format drift into a visible number rather than a silently shorter
list. Neither pads the feed with "unrecognised record" noise, and assistant `thinking` blocks
are never surfaced at all.

Strings are scrubbed through the same `DeterministicCleaner` the public mirror uses, plus one
transcript-specific layer for **unquoted** `NAME=VALUE` shell assignments (including this
fleet's `*_PAT_*` convention) — a shape the mirror cleaner does not target, because in source
files such values are quoted. Treat this as best-effort defence in depth, **not a guarantee**: a
transcript can contain arbitrary text and no pattern set recognises every secret shape. What
actually bounds exposure is structural — summaries are truncated to one short line, tool results
are summarised rather than echoed, and private reasoning is never surfaced.

| Variable | Default | Meaning |
|---|---|---|
| `AGENTSESS_TAIL_BYTES` | `262144` | How much of a transcript tail to read. |

- **`agentsess_capture`** — the recent scrollback of the tmux pane a session is attached to,
  so it can be rendered as a read-only terminal view. Takes a `session_id` (whose pane target
  is *constructed*, not parsed) or an explicit `target`, plus `lines` and `host`.

The pane target is the security boundary: argv form stops shell injection but not **option**
injection, since `tmux` reads a leading-dash argument as a flag. The target is validated
fail-closed against a deliberately narrow `session:window.pane` shape — narrower than tmux's
own rules, because a permissive validator is where a separator or an option would hide.

Output is bounded by **both** line count and total bytes (a wide pane blows the byte budget
long before the line budget), cut on a character boundary, and redacted through the same path
as the transcript reader — a terminal displays credentials as readily as a transcript does.
Both caps report when they fire. A session with no pane is a clear error rather than an empty
capture: "nothing to show" and "not attached to a terminal" are different answers.

| Variable | Default | Meaning |
|---|---|---|
| `AGENTSESS_CAPTURE_MAX_LINES` | `2000` | Scrollback line cap. |
| `AGENTSESS_CAPTURE_MAX_BYTES` | `262144` | Total byte cap. |

**This suite is read-only by design.** Nothing in it writes a file, sends a keystroke, or
signals a process. Being able to *watch* an autonomous agent carries no risk; being able to
*type into* one can alter a build mid-flight. A send capability is a separate, gated change
needing a session allowlist, a control-character whitelist, rate limiting and an audit-log
entry — do not add one here without that gate.

### The OAuth connector door — letting a hosted client in without widening anything

Terminus has three ways in, and all three are private: the loopback listener, the mTLS
listener, and the tailnet listener. Each binds a caller's identity to a transport artifact —
a client certificate CN, or a tailnet WhoIs. That works for the fleet's own services. It
cannot work for a **hosted third-party client**: Anthropic's Claude surfaces reach an
external MCP server over public HTTPS with OAuth 2.1 and cannot present a certificate this
fleet issued.

The `oauth` subsystem is the fourth door. Its output is an ordinary `Principal`, which the
**existing** `gateway_framework` authorization already understands — the new door changes
how a caller proves who they are, never what they may do once in.

**The scoping model, which is the reason this is safe to expose at all.** Every request
resolves to an intersection, and an intersection can only ever *remove*:

```text
effective = grant_of(account)          // what the HUMAN may do  (existing machinery)
          ∩ tools_of(client.groups)    // what THIS connector may do
          ∩ namespaces(client.servers) // which federated servers it sees
```

There is deliberately no path by which a client scoping record grants a tool the account's
own grant would have denied. A client with no scoping record reaches the **empty set**, not
the account's full grant; a tool-group pattern matching nothing is empty, not a wildcard.

**"Only I can link my account" is enforced at consent, not at registration.** Possession of
a `client_id` gets a caller as far as a login screen. Issuing a token needs an argon2id
password sign-in *and* an explicit approval of a named client and a named capability set. A
client nobody consents to holds nothing.

The interactive endpoints (`GET /oauth/authorize`, `POST /oauth/login`, `POST /oauth/consent`)
are server-rendered pages with no JavaScript, no framework and no external resource, served
under `default-src 'none'; frame-ancestors 'none'`. Several of their properties are
load-bearing rather than incidental:

- **Two failures never redirect.** An unknown `client_id` or an unregistered `redirect_uri`
  renders a terminal error page and emits no `Location` header at all — redirecting to an
  unvalidated address is an open redirect. Everything checked afterwards *is* an OAuth error
  redirect, because by then the destination is one an operator registered.
- **Redirect matching is exact, with one named exception.** An RFC 8252 loopback URI matches
  with the port ignored, because a native client binds an ephemeral port it cannot know at
  registration time. That exception requires the scheme to be `http`, no userinfo in the
  authority, an identical host from a fixed three-item allowlist, and identical path, query
  and fragment. Only the port may differ. A general fuzzy match is how open redirects are
  born; this is a branch with its own tests in both directions.
- **A redirect URI that already carries a response parameter is refused.** `…/cb?code=x`
  would produce a response with two `code` parameters and let the *client's* query parser
  decide which wins. Reserved names (`code`, `state`, `iss`, `error`, `error_description`)
  are refused at validation and dropped at build time.
- **PKCE S256 is required**; `plain` **and an omitted method** are both refused — RFC 7636
  makes `plain` the default when the method is absent, so accepting the omission would be
  accepting `plain`.
- **Tokens are audience-bound.** The RFC 8707 `resource` parameter is required and must equal
  the configured canonical resource, so a token minted here cannot be replayed at a federated
  peer.
- **An unknown account and a wrong password are indistinguishable** in body, status and
  *timing* — the no-account path verifies the submitted password against a real argon2id hash
  with the same cost parameters rather than returning early. Both the per-account and the
  per-source rate-limit budgets are consumed before any credential work, so the limiter is
  not an oracle either. The source address comes from the connection, never from
  `X-Forwarded-For`: a rate-limit key the caller can rotate is not a rate limit.
- **Consent shows resolved capabilities, not a scope string.** "mcp" tells a human nothing
  about whether they are approving a weather lookup or a host restart, so the page lists the
  client's resolved tool groups with their patterns and its federated namespaces. An empty
  list is stated in words as *grants no tool access at all* — an empty section reads as
  "unrestricted", which is the exact misreading the scoping model exists to prevent.
- **Loopback clients carry a warning**, as the MCP specification requires: a loopback
  redirect cannot be authenticated, so any process on the user's machine that binds the port
  first receives the code.
- **Authorization codes** carry 366 bits of entropy, are stored only as a SHA-256 digest,
  live 60 seconds, and are bound to client, account, redirect URI, resource, PKCE challenge
  and scope. One authentication yields at most one code: the login session's identifier is
  claimed by an `INSERT … ON CONFLICT DO NOTHING`, so a replayed consent post loses the race
  **across every replica**, not merely within one process.

**Configuration** (names only — values are materialized from the runtime secret store, never
authored by hand): `RMCP_DATABASE_URL`, `RMCP_OAUTH_SIGNING_KEY`, `RMCP_OAUTH_ISSUER`,
`RMCP_OAUTH_RESOURCE`. The schema lives in `migrations/S132-rmcp01-oauth-core.sql` and
`migrations/S132-rmcp03-login-session.sql` and is **not** applied at startup: apply it via
`pg_ddl` as part of the deploy. Until it is, the store reports the door unconfigured rather
than serving a silently dead auth surface.

**Not yet complete.** An account with a TOTP second factor is currently **refused** at
sign-in rather than admitted on its password alone: the stored seed is encrypted with a
subkey nothing derives yet, and a verifier cannot check a code against a seed it cannot
decrypt. That is a deliberate fail-closed gate, not a fault — RMCP-08 provisions the subkey.
Clearing `totp_secret_enc` to work around it would silently downgrade the account to one
factor and must not be done.

### Live viewing activity — `media_now_playing`

Every other media read is historical (library, watch history, on-deck, recently added).
`media_now_playing` is the one **live** read: Plex `/status/sessions`, direct — no
Tautulli, no second client, no new credential. Per session it reports the title (with
show/season/episode where applicable), who is watching, the player, progress and
duration, and the playback decision — **direct play / direct stream / transcode** with
the transcode reason — plus a session count and total bandwidth.

Two properties are load-bearing rather than incidental:

- **Outcomes that never render identically.** *Plex unreachable*, *token rejected*,
  *Plex answered something unreadable* and *nobody is watching* are different facts and
  are kept apart at the type level from the transport upward. Only the last one is an
  `ok` answer (`status: "idle"`, an empty session list); every failed read carries **no
  session count at all**, so it can never be misread as an empty house. That holds for
  the parser too, not just the transport: `idle` is granted only to the one empty shape
  the live server actually emits (`MediaContainer` present, `size: 0`, no `Metadata`),
  and any other body that cannot be walked — a missing `MediaContainer`, a nonzero
  `size` with no sessions attached, a count that disagrees with the list — is
  `malformed`. A broken read that claimed "nobody is watching" would be the worst
  failure this tool could have, because on a dashboard it looks like an answer.
  Stated explicitly for consumers, because *absent*, *null* and *not-a-count* are not
  the same shape:

  > `status` is `idle` only when `MediaContainer.size` is **present** and is the whole
  > number `0`, **and** `MediaContainer.Metadata` is either **absent** or an **empty
  > array**. Everything else is `malformed`, never `idle`:
  >
  > - an explicitly `null` `Metadata`, at every `size` including `0` — Plex omits the
  >   key when nothing is playing (`{"MediaContainer":{"size":0}}`) and emits no JSON
  >   `null` on any endpoint, so a null means the response was rewritten in transit and
  >   neither it nor the `size` beside it can be trusted;
  > - an **absent** `size`, whatever `Metadata` does — Plex states a size on every
  >   container it emits, so a missing one is the same evidence of an altered response;
  > - a `size` that is **fractional, negative, or not a number** — a count of things is
  >   a whole non-negative number, so `0.5` is not an imprecise count but an impossible
  >   one, and it is never floored to `0`;
  > - a session whose `TranscodeSession` is present but is **not an object** (`null`, a
  >   scalar, a list) — the same evidence and the same verdict one level down, and the
  >   whole response fails rather than that one session, because dropping it would be an
  >   undercount and keeping it would state a playback decision derived from a payload
  >   that was demonstrably rewritten.
- **`transcode_reason` may be null for a non-direct-play session.** It is non-null *iff*
  `decision != "direct_play"` **and** the session carried a `TranscodeSession` **object**.
  Plex sometimes states the decision on `Media[0].Part[0]` with no transcode session to
  explain it, and no reason is invented for that case — a field whose job is to state a
  reason must not carry a guess. A `TranscodeSession` that is present but is not an
  object is never read as one: it fails the whole response as `malformed` (above), so a
  rewritten transcode block can never surface as a confident decision with a manufactured
  reason. Consumers read `decision` as the discriminant for playback mode and render a
  null reason as "no reason given".
- **Numeric fields split by meaning.** `season`, `episode` and `year` are ordinals: a
  fractional value is dropped to `null` rather than truncated, so a consumer is never
  shown an episode number the payload did not state. `progress_ms`, `duration_ms` and
  `bandwidth_kbps` are measurements: a fractional value is rounded to the nearest whole
  unit, because a half-millisecond is not worth a blank progress bar. `session_key` is
  an opaque string and is never read as a number.
- **A `malformed` detail never quotes the payload back.** Every verdict above rests on
  *this response was rewritten in transit, so nothing in it can be trusted* — which makes
  echoing the offending value into our own error the one thing not to do: it hands whoever
  rewrote the response a channel into the structured payload (a title, a username, an
  address or a credential parked in `size` came straight back out, to a caller who may be
  entitled to none of it) and lets an arbitrarily long value produce an arbitrarily long
  error. So a detail names a **safe category** — the JSON type that arrived and the type
  that was expected — and carries a value only when that value is a **number**, whose
  rendering is hard-bounded and which cannot carry a name, an address or a secret. That
  exception exists because `size: 0.5` is the one fault where the value *is* the
  diagnosis. Every detail is additionally capped in length on the way out, as defence in
  depth rather than as the mechanism. This matches what the rest of the path already
  does: transport failures are **classified**, never `Display`ed, and the Plex base URL
  and token are never echoed anywhere.
- **Private by default.** Now-playing reveals who is home and what they are doing, in
  real time, so it is gated on the caller's entitlement (the same `CallerContext`
  mechanism `weather` uses for operator-context inference). A guest, an unknown caller,
  a caller with no principal, or any un-threaded dispatch path receives `forbidden` and
  nothing else — no titles, no usernames, no device names, and no count, because a count
  alone discloses occupancy. The gate runs *before* the client is touched, so an
  unentitled call issues no Plex request at all. It is deliberately **not** in the guest
  baseline.

### The OAuth 2.1 connector door (`oauth`)

Terminus's three existing doors — the loopback listener, mTLS, and the tailnet — all bind
a caller's identity to a transport artifact: a client-certificate CN, or a tailnet WhoIs.
That works for the fleet's own services and for machines the operator enrolled by hand. It
cannot work for a hosted third-party client, which reaches an external MCP server over
public HTTPS as an **OAuth 2.1 public client with PKCE**. This subsystem is the fourth
door. It changes how a caller proves who they are; it does not introduce a second way to
decide what they may do — every request still resolves to an existing `mesh::Principal`,
and per-client scoping can only ever *narrow* the account's own grant.

**`POST /oauth/token`** (`oauth::token`) implements both grants. Three properties carry the
security of the whole door:

- **The body is `application/x-www-form-urlencoded`, checked here rather than delegated to
  a framework extractor.** Hosted clients send both the initial exchange and every refresh
  that way; a JSON-only parser answers with a bare `415` carrying no `error` field, which a
  client cannot distinguish from a broken server — every connection would fail identically
  and silently. A repeated parameter is refused rather than last-write-wins, so a proxy and
  this origin can never disagree about which `resource` or `scope` was authorized.
- **Access tokens are audience-bound.** The JWT's `aud` is the RFC 8707 `resource` bound to
  the *code* (or to the refresh token's family), never a value taken from the request — a
  `resource` parameter may only *agree* with the binding, never establish one. That is what
  stops a token minted for a federated peer being replayed at this server. Verification
  requires the caller to state which audience it is; there is no "any audience" mode.
- **Refresh tokens rotate, and reuse is treated as theft.** Presenting an already-rotated
  token revokes the entire family and returns exactly `invalid_grant`: the legitimate
  holder and the thief cannot be told apart, so both are cut off and the human
  re-authorizes. Every error is a registered RFC 6749 code, because a hosted client keys
  its re-authorization on `invalid_grant` and a custom code strands the user permanently.

Ordering is part of the design. Client authentication runs *before* the authorization code
is touched, so an unauthenticated caller cannot burn codes it does not own; the code is then
consumed *before* the PKCE, redirect and resource checks, so a stolen code cannot be retried
with different parameters until one combination is accepted. Single-use and rotation are
decided in SQL (`oauth::store`) — one conditional `UPDATE` and one transaction — and nothing
in the endpoint re-implements them, because a read-then-write there would reopen exactly the
races the store closes. PKCE is compared in constant time, and a code somehow persisted
without a challenge is **not** exchangeable.

Nothing here stores a presentable credential: codes and refresh tokens are high-entropy
values kept as SHA-256 digests, client secrets and passwords as argon2id PHC strings. Keys
and lifetimes come from the vault-materialized environment (`RMCP_OAUTH_SIGNING_KEY` and the
other `RMCP_*` names in `.env.example`); verification accepts a *previous* signing key for a
rotation grace window, while minting always uses the current one.

The full inventory (17 subsystems, plus `compiler`, `constellation-web`, `compat`,
and the crate-root modules) is in [docs/reference/index.md](docs/reference/index.md).

## The public MCP connector edge

Terminus's three original doors — the loopback plain listener, the mTLS
listener, and the tailnet listener — are all private and all bind a caller's
identity to a transport artifact (a client-certificate CN, a tailnet identity).
A hosted third-party client can present neither.

`RMCP_EDGE_ENABLED` adds a fourth listener inside `terminus_primary`
(`src/oauth/edge.rs`) for exactly that case: an internet-facing door, behind a
TLS-terminating reverse proxy, exposing **only** the two OAuth `.well-known`
documents, `/oauth/*`, and `/mcp`. Everything else the router serves —
`/enroll`, `/admin/*`, `/healthz`, the inference routes — is unreachable through
it, and a path with no policy entry is denied rather than defaulted to open.

The policy is per-**path**, not per-host, because the two halves of an OAuth
flow arrive from different networks: Anthropic fetches discovery, `/oauth/token`
and `/mcp` from its published egress range, while `/oauth/authorize` opens in the
operator's own browser and never comes from Anthropic. A single "allow only
Anthropic" pinhole serves discovery perfectly and then silently 403s the person
trying to consent — which is the failure this design exists to avoid.

The listener is off unless configured, and an unusable policy is a hard startup
error rather than a permissive default. See
**[docs/networking/remote-mcp.md](docs/networking/remote-mcp.md)** for the
runbook: the configuration surface, the reverse-proxy and TLS layout, how the
client address is resolved (and the two proxy misconfigurations that break it),
and a troubleshooting table. Deploy assets live in
[`deploy/rmcp-edge.service`](deploy/rmcp-edge.service) and
[`deploy/rmcp-edge-proxy.conf.example`](deploy/rmcp-edge-proxy.conf.example).

## Quick Start

```sh
git clone <your-remote>/Terminus && cd Terminus
cargo build --release                      # workspace: terminus-rs, terminus-client, terminus-worker-sdk
cargo run --bin terminus_primary           # gateway; binds 127.0.0.1, port from TERMINUS_PRIMARY_PORT (default 8310)
```

Configuration is entirely env-sourced — key names only here; values are
materialized from the vault at runtime, never committed. The minimum useful set:

- `TERMINUS_PRIMARY_PORT` / `TERMINUS_PRIMARY_BIND` — gateway listener (loopback by default).
- `PLANE_API_URL`, `PLANE_API_KEY` — Plane tools (tools register regardless and return `NotConfigured` until set).
- `GITEA_URL`, `GITEA_PAT_<NAME>`, `GITEA_IDENTITY_NAME` — Gitea tools (named-identity model).
- `GITHUB_PAT_<NAME>` — GitHub tools; `POSTGRES_URL_<NAME>` — `pg_*` connection identities.
- `REVIEW_DAEMON_URL`, `REVIEW_DAEMON_TOKEN` — CLI-backed review providers (run `review_daemon` separately).

### The remote-MCP connector URL (OAuth door)

`terminus_primary` can additionally expose an OAuth 2.1 door for hosted MCP clients
(RMCP). It is **off** unless `RMCP_CANONICAL_RESOURCE` is set, and when it is set the
value has a contract that is worth reading once, because getting it slightly wrong
fails in a way the client cannot describe.

| Key | Meaning |
|---|---|
| `RMCP_CANONICAL_RESOURCE` | **The connector URL, byte-for-byte as typed into the client's connector form.** Enables the door. |
| `RMCP_ISSUER` | OAuth issuer identifier. Defaults to the canonical resource's origin. |
| `RMCP_SCOPES_SUPPORTED` | Space-separated advertised scopes. Default `mcp offline_access`. |
| `RMCP_REQUIRED_SCOPE` | Scope an access token must carry to reach `/mcp`. Default `mcp`. |
| `RMCP_DCR_ENABLED` | Advertise and accept RFC 7591 dynamic client registration. Default off. |

**The contract.** `RMCP_CANONICAL_RESOURCE` is published verbatim as the `resource`
field of the protected-resource metadata document, is echoed by the client as the
RFC 8707 `resource` parameter, and becomes the audience of every issued token. Those
three strings are compared byte-for-byte. The server therefore **does not normalize
it**, and refuses at startup — with a message naming the variable — anything it would
otherwise have had to normalize:

- must be an absolute `https://` URI, with a lowercase scheme;
- **no trailing slash** (`https://host/mcp` and `https://host/mcp/` are different
  audiences — this is the single most common cause of a connector that authorizes and
  then fails every call);
- no fragment, no query string, no userinfo, no whitespace or non-ASCII.

A malformed value **aborts startup**. That is deliberate: a *nearly* correct value
starts fine, serves a document the client fetches happily, and then fails at token
issuance with a client-side message — "Couldn't reach the MCP server" — that names
neither the field nor this server.

**The discovery contract.** Three unauthenticated endpoints implement it, all served
from bodies rendered once at startup (no database, so discovery answers even when the
store is down), and all answering `HEAD` as well as `GET`:

| Path | Document |
|---|---|
| `/.well-known/oauth-protected-resource` | RFC 9728 protected-resource metadata |
| `/.well-known/oauth-protected-resource/<resource path>` | The same document. Clients probe **this** form first. |
| `/.well-known/oauth-authorization-server` | RFC 8414 authorization-server metadata |

An unauthenticated `POST /mcp` answers `401` with
`WWW-Authenticate: Bearer realm="…", resource_metadata="…", scope="…"`. That header is
the entire discovery bootstrap and is honoured **only** on a `401` — a client discards
it on a `200`. A valid token that lacks a required scope answers `403` with
`error="insufficient_scope"` and the scopes needed, which is a different instruction to
the client than `401`: re-authorize for more scope, rather than discard the credential.

Then connect any MCP client to the endpoint and call `initialize` /
`tools/list` / `tools/call` (JSON-RPC 2.0 over streamable HTTP; `GET /healthz`
for liveness). Full walkthrough: [docs/getting-started.md](docs/getting-started.md).

## Documentation

| Page | What it covers |
|---|---|
| [docs/index.md](docs/index.md) | Documentation hub: full navigation with per-page descriptions |
| [docs/getting-started.md](docs/getting-started.md) | Clone → build → run `terminus_primary` → connect an MCP client |
| [docs/architecture.md](docs/architecture.md) | Full derived diagram, per-subsystem narrative, and how a tool call flows |
| [docs/reference/index.md](docs/reference/index.md) | Subsystem inventory; 13 deep reference pages with real symbols and config keys |
| [docs/guides/index.md](docs/guides/index.md) | Operator guides: model-intake sweeps, review panels, the git-public mirror |
| [docs/tools/README.md](docs/tools/README.md) | Existing per-tool documentation, grouped by domain |
| [docs/architecture/](docs/architecture/mesh.md) | Existing deep dives: auth, broker, chord-integration, federation, mesh |
| [docs/networking/remote-mcp.md](docs/networking/remote-mcp.md) | The public MCP connector edge: per-path source policy, reverse proxy + TLS, troubleshooting |
| [docs/build.md](docs/build.md) | Build pipeline notes; see also [docs/house-style.md](docs/house-style.md) |

## At a glance

- 10,064 functions · 1,108 structs · 161 traits · 161 enums across 410 modules (11,905 KG nodes, 27,107 edges).
- 381 core tools (`register_all`) + 189 personal tools (`register_personal`; 3 not also in core).
- 12 binaries, including `terminus_primary` (gateway), `terminus_personal`, `review_daemon`, `mint`, `pii_gate`, `cortex_calibrate`, `house_style_check`.
- 3 workspace crates: `terminus-rs`, `terminus-client` (enrollment + mTLS transport), `terminus-worker-sdk` (worker authoring).
- Top call-graph hotspots: `mesh::principal::PrincipalResolver::map`, `registry::ToolRegistry::contains`, `mesh::tailnet::TailnetServer::start`.

## The `pii_gate` binary

The authoritative PII pre-push / pre-commit gate, replacing the legacy
`.githooks/pii_gate.py`. It scans git *objects* (committed or staged blobs), not
the working tree, so a secret that is committed but since deleted is still
caught. It fails **closed**: a git error, a missing binary, or a malformed
config is never reported as a clean push.

```sh
cargo build --release --bin pii_gate
ln -sf ../../target/release/pii_gate .git/hooks/pre-push
```

| Flag | Effect |
| --- | --- |
| *(none)* | git pre-push protocol on stdin — scans the blobs being pushed |
| `--staged` | git pre-commit — scans the staged index |
| `--tree [PATH]` | sweeps a whole working tree (used by the mirror engine) |
| `--json` | machine-readable report instead of the human summary |
| `--visibility <internal\|public>` | override the repo's declared posture |
| `--posture` | print the resolved posture and why, then exit 0 |

### Posture: internal vs public

The scan is always full-strength; posture decides only which categories are
**reported**. A repo declares it via `[repository] visibility` in
`.moosenet-repo.toml`.

- **`internal`** — the fleet's own infrastructure *identifiers* are not reported:
  container ids, internal hostnames and domains, operator paths, uuids, phone
  numbers, the operator's name, and infra service names. An internal repo
  legitimately documents these.
- **`public`** — everything is reported. This is also the fail-closed default
  when the declaration is absent, unparseable, or has an unrecognized value.

**Real credentials are never posture-gated.** Private IPs, API keys, JWTs, PEM
private keys, cloud provider keys, and quoted secrets fire at every posture, as
do any operator-configured `extra_terms` / `extra_patterns`.

This mirrors the `EXTENDED_PATTERNS` split of the Python gate it replaces. The
filter lives in the binary only — the shared `ruleset_from_config` seam used by
the runtime write gate and the git-public mirror engine stays unconditionally
full-strength, so mirroring is never weakened by a repo's posture.

Repo-specific terms, extra patterns, allowed emails, and exclusions come from a
repo-root `pii-gate.toml` (or the path in `TERMINUS_PII_CONFIG`).

## Contributing

Every code change goes through the constellation build pipeline: spec item →
worktree → test gate (including `house_style_check` and the `pii_gate` hooks) →
dual review → merge. See [docs/build.md](docs/build.md).

## License

MIT — see [LICENSE](LICENSE).
