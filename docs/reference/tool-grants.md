# Tool grants — who may call what

`src/gateway_framework` — the per-identity grant map every request passes
through. This page is the reference for the **grant model**: how a principal is
mapped to a set of callable tools, what the grant shapes mean, how the
guest/family baseline is built, and how to add a principal.

Grants are enforced in exactly one place — `GatewayFramework::guard()` — and
consulted (without side effects) in two more: `tools/list` filtering
(`filter_catalog_for_principal`) and the agent tool router's selection step
(`permits_tool`). All three resolve the same `AllowlistPolicy::is_allowed`
decision for the same action string, so **a tool a principal cannot call is
also a tool it is never shown**. That parity is the point: the assistant should
not surface tools the caller is not authorized to use.

## The model in one paragraph

A **principal** (`crate::mesh::Principal` — an mTLS client-cert CN, a tailnet
WhoIs identity, or a named-PAT identity, reconciled to one canonical name) is
looked up in the grant map. Its **grant** answers one question: may this
principal perform this **action**? An action is a tool name (`weather`,
`upstream__ledger_add`), an inference route (`/v1/chat/completions`), or an
`admin:`-namespaced control-plane op. **Default-deny**: a principal with no
entry at all is denied every action. There is no global allowlist with
per-identity exceptions — the map is per-identity from the start.

## Grant shapes

Configured as JSON in `TERMINUS_GATEWAY_ALLOWLIST_JSON`, an object of
`identity -> grant`. Two shapes are accepted:

```jsonc
{
  // List form: a plain allow list. No deny layer exists on this shape.
  "operator": ["*"],
  "reporting": ["ledger_accounts", "vitals_today"],

  // Allow/deny form: an allow list minus deny PREFIXES, which win even over
  // an allow "*". This is what makes "broad access except the sensitive
  // stuff" expressible without hand-listing ~300 tool names.
  "some-service": {
    "allow": ["*"],
    "deny":  ["github_", "infisical_", "approval_"]
  }
}
```

### Entry forms

| Form | Side | Meaning |
|---|---|---|
| `"*"` | allow | Every action. |
| `"<prefix>*"` | allow | Every action starting with `<prefix>` — e.g. `"news_*"`, or `"upstream__*"` for one whole mesh upstream. |
| `"exact_tool_name"` | allow | That action only. |
| `"<prefix>"` | deny | Literal prefix: matches an action equal to it **or** starting with it. `"github_"` covers `github_push_repo`, `github_list_repos`, … |

Two asymmetries are deliberate and worth internalising:

- **Deny entries are literal prefixes and take no `*`.** `deny: ["github_*"]`
  matches *nothing* (there is no globbing on the deny side), which reads as
  "block the family" and does the opposite. Writing a `*` in a deny entry is
  now a **rejected** config (see [Validation](#validation--fail-closed)).
- **Deny is checked against the namespaced action AND its bare name.** A
  sensitive tool re-exported through a mesh upstream
  (`someupstream__github_push_repo`) is still caught by the bare `"github_"`
  deny prefix. The **allow** side has no such bare-name fallback — an allow
  entry matches the action verbatim. So granting a tool that arrives through a
  mesh namespace requires naming the namespace (`"someupstream__weather"` or
  `"someupstream__*"`). That direction is fail-closed by design: forgetting it
  denies access, it never grants extra.

### Admin actions are separately scoped

An `ActionKind::Admin` action (`admin:register_worker`, …) is authorized only
by an **admin-namespaced** entry — an exact `"admin:<op>"`, or a wildcard whose
prefix is itself admin-scoped (`"admin:*"`, `"admin:reg*"`). A bare `"*"` tool
wildcard never authorizes an admin op. A broad tool identity is not, by that
fact alone, a worker-control admin.

## The three built-in postures

| Posture | Applies to | Shape |
|---|---|---|
| **Scaffolded service** | `lumina`, `harmony` (`SCAFFOLDED_IDENTITIES`) | `allow: ["*"]` minus `DEFAULT_SENSITIVE_DENY_PREFIXES` |
| **Guest / family** | identities named in `TERMINUS_GATEWAY_GUEST_IDENTITIES` | `GUEST_BASELINE_ALLOW` (exact names) plus the same sensitive deny layer — and it is a **ceiling**, see below |
| **Operator** | whatever the operator configures, conventionally `["*"]` | List form, unrestricted — see [the wildcard question](#the-legacy-wildcard-question) |

Precedence at load: scaffold defaults → guest baseline → `TERMINUS_GATEWAY_ALLOWLIST_JSON`.

For a **non-guest** identity the env JSON wins **per identity, in full** — it
replaces that identity's grant, it is never merged with the default.

For a **guest** identity it does not. Naming an identity in
`TERMINUS_GATEWAY_GUEST_IDENTITIES` is a classification, and that classification
is an **upper bound**: the explicit entry is *intersected* with
`GUEST_BASELINE_ALLOW`, so it may **narrow** a guest but can never widen one.
See [Guest classification is a ceiling](#guest-classification-is-a-ceiling).

## The guest / family baseline

> ### ⚠ Scope: what the guest baseline does **not** protect (TERM #577)
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
> neither this list nor the weather entitlement gate applies to them. The same
> limit applies to the per-principal tool cache: it isolates `lumina` from
> `guest-alex`, not two humans who are both `lumina`.
>
> **Provisioning guest identities without closing TERM #577 gives a FALSE sense
> of containment.** The guest surface is real only for a separately
> authenticated principal. Closing the gap needs end-to-end human-identity
> propagation — design work tracked as **TERM #577** (a blocker for the `hearth`
> family sprint), not a wider or cleverer grant map.

Set `TERMINUS_GATEWAY_GUEST_IDENTITIES` to a comma-separated list of identity
names and each gets the baseline grant:

```
TERMINUS_GATEWAY_GUEST_IDENTITIES=guest-alex,guest-sam
```

Today's surface (`GUEST_BASELINE_ALLOW`, exact names) — **ten entries: one
inference route plus nine tools.** The route is not a tool and never appears in
`tools/list`, so the two counts differ and both are given explicitly wherever
they are quoted below.

| Entry | Why it is safe for a non-operator |
|---|---|
| `/v1/agent/execute` | The assistant turn itself — a guest must be able to *talk* to Lumina or the tool grant is inert. Every tool the router dispatches inside the turn is re-checked against this same grant, so the route grants conversation, not reach. The raw completion routes (`/v1/chat/completions`, …) are **not** granted: those bypass per-principal tool selection and let the caller pick the model and prompt. A guest gets the assistant, not the engine. |
| `time_now` | The authoritative fleet clock. No arguments reach a backend, nothing is read or written. |
| `weather` | Public third-party forecast for a location the caller supplies **explicitly**. The tool *can* otherwise infer an omitted location from the operator's calendar or home/work routine; that inference is gated on `CALENDAR_CONTEXT_PROBE`/`ROUTINE_CONTEXT_PROBE`, neither of which is granted here. What makes it safe for a guest principal is the explicit-location-only path, **not** an absence of household data in the tool — and see the scope warning above for who is (and is not) a guest principal. |
| `news_headlines`, `news_search`, `news_topic` | Public news retrieval, read-only. |
| `media_search`, `media_recommend`, `media_recently_added`, `media_on_deck` | Media **discovery** — browsing the catalogue. |

**The baseline is an allowlist, and that is load-bearing.** A denylist-shaped
guest grant (`allow: ["*"]` minus sensitive prefixes — the scaffolded service
posture) would mean every tool family added to Terminus in future is granted to
guests the day it registers, and stays granted until someone remembers to deny
it. That is exactly backwards for the least-trusted principal on the system. A
guest sees a new `thermostat_*` or `doorlock_*` family only after a deliberate
edit to `GUEST_BASELINE_ALLOW`.

For the same reason the entries are **exact names, not prefixes**: `"media_*"`
would sweep in `media_request` (acquisition — spends household bandwidth and
disk via the media-acquisition write path), `media_delete`, `media_organize`, and
`media_taste_feedback` (writes a personal taste profile) alongside the four
discovery tools. Deliberately excluded, and they must stay excluded even as the
`media_` family grows.

The baseline still carries `DEFAULT_SENSITIVE_DENY_PREFIXES` underneath. That
is redundant today — no allowed entry could match a sensitive prefix — and is
kept as defence in depth for the predictable future edit where someone widens
the allow set or copies this grant as the starting point for a new household
role.

A guest identity has no admin grant, so no control-plane op is reachable — and
that holds regardless of what `TERMINUS_GATEWAY_ALLOWLIST_JSON` says about it,
per the ceiling below. Because guests are still principals, the operator-guarded
tool set (`approval_*` and everything `crate::approval::is_guarded` covers) is
blocked unconditionally in the router regardless of any grant.

### Guest classification is a ceiling

**An identity named in `TERMINUS_GATEWAY_GUEST_IDENTITIES` can never resolve to
more than `GUEST_BASELINE_ALLOW`, whatever `TERMINUS_GATEWAY_ALLOWLIST_JSON`
says about it.** An explicit entry for a guest is *intersected* with the
baseline (`clamp_to_guest_ceiling`), not substituted for it:

| Explicit entry for `guest-alex` | Effective grant |
|---|---|
| *(none)* | the full baseline |
| `["weather"]` | `["weather"]` — **narrowing works, and is the point of writing an entry** |
| `["*"]` | exactly the baseline — clamped, and logged |
| `{"allow": ["google_calendar_today", "commute_estimate"], "deny": []}` | *nothing* — every entry is outside the ceiling |
| `["weather", "infisical_get_secret"]` | `["weather"]` |
| `["admin:*"]` | *nothing*; a guest never holds an admin grant |

This closes a real hole. Before it, an explicit entry replaced the baseline in
full, so `{"guest-alex": ["*"]}` — one wildcard, typed once, or a line
copy-pasted from an operator identity two lines above — gave a houseguest
`google_calendar_today` and `commute_estimate`. `GatewayFramework::caller_context`
then minted an **entitled** context for them, and `weather` answered an omitted
location with the operator's calendar event summary or configured home/work
address. A protection whose entire job is to bound what a guest can reach must
not be escapable by editing the config it is supposed to bound.

**Why intersect rather than reject the entry outright.** A malformed grant is
denied outright (below) because it has no legible meaning — there is nothing to
honour. A widening grant *is* legible: every baseline tool it names is an intent
we can honour exactly, so intersecting satisfies the invariant while still doing
what you asked wherever that is permissible. It also fails in the recoverable
direction — a clamped guest keeps working, a denied guest is an outage for a
household member who did nothing wrong. The security property is identical
either way; the tie breaks on operability.

**A clamp is never silent.** It is logged at `warn`, naming the identity, the
entries dropped as outside the baseline, the effective allow list, and the fact
that guest classification is a ceiling — so an operator who wrote a wider grant
learns it was reduced rather than quietly getting something other than what they
wrote. **To grant an identity more than the baseline, remove it from
`TERMINUS_GATEWAY_GUEST_IDENTITIES`** — it is then not a guest, and its entry
applies in full like any other.

The ceiling composes with the fail-closed validation below rather than softening
it: a *malformed* entry for a guest still denies that guest outright, it does not
fall back to the clamped baseline.

## Validation — fail-closed

Every entry in `TERMINUS_GATEWAY_ALLOWLIST_JSON` is validated at load. **A
malformed grant is never treated as allow-all, and never leaves the identity at
whatever it had before**: the entry is dropped AND any scaffold/guest baseline
the seeding pass gave that identity is revoked, so the identity ends up
default-denied. The failure is logged at `error`, naming the identity, the
reason, and the fact that the identity is now denied.

That last part is the point. The map is built by seeding the scaffolded
(`lumina`, `harmony`) and guest identities first and applying your explicit
entries on top, so "skip the bad entry" would mean an operator who writes an
entry to *narrow* `lumina` and mistypes the JSON shape silently keeps the full
`allow: ["*"]` scaffold — the fail-open direction, invisible from behaviour.
Writing an entry for an identity is an expression of intent to control it; if
that intent cannot be parsed, the identity is denied rather than left broader
than you wrote. A wrong denial is loud and fixed in seconds; a silently retained
wildcard is not detectable at all. Only the offending identity is affected —
one bad entry never invalidates the rest of the map.

The same applies to a malformed identity *key*: `{" lumina": [...]}` grants
`lumina` nothing (keys are rejected, never trimmed into a grant nobody wrote)
**and** revokes `lumina`'s seeded scaffold, because the intent to configure
`lumina` is legible even though the key is not usable.

One deliberate exception: if the **whole** JSON fails to parse there is no
per-identity intent to read, so the scaffold is retained (denying every
scaffolded identity on any JSON typo would take the fleet down rather than
narrow it). It is logged at `error`.

Rejected:

- Any JSON type other than an array of strings or an `{"allow":…,"deny":…}`
  object.
- **An unknown key on the object form.** This is the case that motivated the
  validation: `{"allow": ["*"], "denny": [...]}` previously deserialized
  cleanly into `allow: ["*"], deny: []` — an unrestricted wildcard grant
  produced by a typo, with no error anywhere.
- An empty entry, or one containing whitespace (it could never match a real
  tool name or route).
- A `*` in a **deny** entry (see the asymmetry above — it silently matches
  nothing).
- A `*` anywhere but as a single trailing character in an **allow** entry
  (`"a*b"`, `"**"`) — the matcher only understands `"*"` and `"<prefix>*"`.

Validation is **per identity**: one bad entry denies that identity rather than
discarding every other identity's config. A top-level JSON parse failure still
falls back to the scaffold-only policy (deny-all except the two service
defaults).

Separately, any identity holding an **unrestricted wildcard** grant
(`["*"]`, or `{"allow":["*"],"deny":[]}`) is logged at `warn` on startup. That
is not an error — see below — it is made visible rather than implicit.

## The legacy wildcard question

`Grant::List(["*"])` has **no deny layer at all**. `DEFAULT_SENSITIVE_DENY_PREFIXES`
constrains only `Grant::AllowDeny`, so a list-form wildcard identity — which is
what the operator identities conventionally use — bypasses every sensitive deny
prefix. This is why the approval mechanism needed a *separate*, unconditional
block in the router (`agent_router::is_model_blocked`) rather than relying on
the `approval_` deny prefix.

**Current behaviour is intentional and unchanged.** These are the operator's own
identities; the operator is not a subject of the sensitive carve-outs, which
exist to keep *service* and *guest* identities away from operator-scoped
surfaces. Silently applying the deny prefixes to them would remove the
operator's access to their own forge, secrets, and fleet-ops tooling from a
config that has not changed — a plausible way to lock yourself out of the
fleet mid-incident.

**Recommendation (not applied here; needs an operator decision):** migrate the
operator identities from the list form to the explicit equivalent —

```jsonc
{ "operator": { "allow": ["*"], "deny": [] } }
```

— which is byte-for-byte the same authorization, but says "unrestricted, on
purpose" in the config rather than by omission of a feature. Then the rule
becomes uniform ("every grant has a deny layer; the operator's is empty by
choice"), the `is_unrestricted_wildcard` startup warning becomes the one signal
to audit, and a future decision to narrow the operator surface is a config edit
rather than a code change. The `agent_router` hard block stays regardless — it
is defence in depth against a misconfigured allowlist, not a substitute for one.

## Worked example — adding a principal

Adding a household member, `guest-alex`, who should get the safe surface plus
the household meal-planning tools.

1. **Enroll the identity.** The principal name is whatever the transport
   presents — for mTLS, the client-cert CN. Issue the cert through the normal
   `crate::pki` enrollment path; the name it carries (`guest-alex`) is the key
   everything below uses. No grant work happens here, and until step 2 the new
   identity is default-denied — enrolling is not authorizing.

2. **Give it the baseline.** Add it to the guest list in the service's
   environment (materialized from the vault like every other value — never
   committed):

   ```
   TERMINUS_GATEWAY_GUEST_IDENTITIES=guest-alex
   ```

   At this point `guest-alex` holds all ten baseline entries: the
   `/v1/agent/execute` route — which is what lets them open an assistant turn at
   all — plus the nine baseline **tools**, and nothing else. `tools/list` shows
   exactly those nine tools; the route is not a tool and never appears there, so
   a catalog of nine is the expected result, not a missing entry.

3. **Add the extra tools, if any — and note that this is where the ceiling
   bites.** While `guest-alex` is listed in `TERMINUS_GATEWAY_GUEST_IDENTITIES`,
   an explicit entry is intersected with `GUEST_BASELINE_ALLOW`, so
   `hearth_recipe_search` in the JSON below would simply be **clamped away**
   (and logged) — it is not in the baseline. A guest entry can narrow, not
   extend. There are exactly two correct moves:

   - **Preferred: widen the baseline itself.** If the household meal-planning
     tools are safe for *every* guest, add them to `GUEST_BASELINE_ALLOW` in
     `src/gateway_framework/mod.rs` (a deliberate, reviewed code edit — which is
     the property the allowlist exists to force) and every guest gets them. Keep
     the entries **exact names**: resist `"hearth_*"`, which would sweep in
     `hearth_pantry_add`/`hearth_shopping_list` (household state writes).

   - **Otherwise: this principal is not a guest.** Remove it from
     `TERMINUS_GATEWAY_GUEST_IDENTITIES` and grant it explicitly, restating the
     baseline alongside the additions — at which point the entry applies in full,
     and so does the responsibility for what it names:

     ```jsonc
     {
       "alex-workstation": {
         "allow": [
           "/v1/agent/execute",
           "time_now", "weather",
           "news_headlines", "news_search", "news_topic",
           "media_search", "media_recommend", "media_recently_added", "media_on_deck",
           "hearth_recipe_search", "hearth_what_can_i_make"
         ],
         "deny": ["github_", "infisical_", "approval_", "ansible_"]
       }
     }
     ```

     Do **not** name `google_calendar_today` or `commute_estimate` here unless
     you mean it: those grants are what `GatewayFramework::caller_context` reads
     to decide whether a tool may fold the operator's calendar or home/work
     addresses into an answer. The `deny` block is redundant against this allow
     list and is kept as defence in depth for later edits.

   Narrowing, by contrast, works normally on a guest —
   `{"guest-alex": ["weather"]}` gives them `weather` and nothing else.

4. **Restart the service.** Grants are read at startup, like every other
   `Environment=` knob.

5. **Verify.** Connect as the new identity and call `tools/list` — it must show
   the granted set and nothing else. Then call one denied tool and confirm a
   `403` with `identity 'guest-alex' is not allowlisted for '<tool>'`, and that
   the denial appears in the audit log. Both halves matter: visibility and
   enforcement are checked separately because they are separate code paths that
   are only *supposed* to agree.

## Key symbols

| Symbol | Kind | File | Description |
|---|---|---|---|
| `AllowlistPolicy` | struct | `src/gateway_framework/mod.rs` | The grant map; `is_allowed`, `is_allowed_admin`, `filter_tools`. |
| `Grant` | enum | `src/gateway_framework/mod.rs` | `List` (legacy, no deny layer) or `AllowDeny`. |
| `validate_grant` | fn | `src/gateway_framework/mod.rs` | Fail-closed per-identity config validation. |
| `GUEST_BASELINE_ALLOW` / `guest_baseline_grant` | const / fn | `src/gateway_framework/mod.rs` | The guest/family surface — and the ceiling a guest's explicit grant is clamped to. |
| `clamp_to_guest_ceiling` | fn | `src/gateway_framework/mod.rs` | Intersects a guest's explicit grant with `GUEST_BASELINE_ALLOW`, so an override can narrow but never widen. |
| `SCAFFOLDED_IDENTITIES` / `DEFAULT_SENSITIVE_DENY_PREFIXES` | consts | `src/gateway_framework/mod.rs` | The `lumina`/`harmony` service posture. |
| `is_model_blocked` | fn | `src/agent_router/mod.rs` | Unconditional operator-guarded block, independent of any grant. |

## Related

- [mesh](mesh.md) — how a `Principal` is resolved from the transport.
- [broker](broker.md) — the `admin:`-namespaced control plane grants gate.
- [Tool availability](../../README.md#tool-availability--parking-a-tool-without-removing-it)
  — the principal-independent filter that composes with grants.
