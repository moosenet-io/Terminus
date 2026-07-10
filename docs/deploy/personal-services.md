# Running your own personal services: `terminus_personal`

<img src="../../assets/personal-services-federation.svg" alt="Your terminus_personal deployment federated into the fleet via Chord's relay" width="100%">

`terminus_personal` is the second of the two Rust Terminus binaries
([`src/bin/terminus_personal.rs`](../../src/bin/terminus_personal.rs)): a
standalone deployment that serves **your own** admin/utility tool set
(`registry::register_personal` — ledger, vitals, git-private, ansible, dev,
and the rest of the personal-registry subset) rather than the fleet's shared
core tools (`registry::register_all`, served by `terminus-primary` — see
[`docs/deploy/client.md`](client.md)). Think of it as running your own
personal MCP server that happens to be reachable by the rest of the fleet,
under your own identity, without ever handing the fleet your credentials.

This page covers standing one up: its own secret-store bootstrap, why its
tool registry is kept separate from the core one instead of merged, how it
gets exposed to fleet callers, and the identity model that makes "your
deployment acts as its own identity" actually true rather than just a
policy statement.

## Why a separate registry, not a combined one

`terminus-primary` (the fleet's aggregated-core gateway) registers *only*
`register_all`. It deliberately does **not** also call `register_personal`
locally, because `register_all` and `register_personal` both register tool
modules (`plane`, `gitea`, `github`, `sundry`) under the **same names** — a
single combined `ToolRegistry` would silently drop one side's entries via
each module's own duplicate-registration handling. Building a combined
registry in the same process is therefore not an option; keeping the two
registries in two separate processes, reached via federation (below), is the
actual fix — see
[`src/bin/terminus_primary.rs`](../../src/bin/terminus_primary.rs)'s module
doc for the full rationale, and `registry::core_personal_name_collisions` for
the regression test that pins this.

## Federation, not merging: how your tools reach the fleet

Your `terminus_personal` deployment doesn't need to be directly reachable by
every fleet caller. Instead, `terminus-primary` reuses Chord's existing relay
(`POST /v1/personal/tools/list` / `POST /v1/personal/tools/call` — see
[`src/federation/mod.rs`](../../src/federation/mod.rs)) as the client side of
a hop that already terminates at your deployment. When a caller asks
`terminus-primary` for a tool name that isn't in its own local core registry,
it forwards the call to Chord, which forwards it on to your
`terminus_personal` process, and relays the result back.

Two authentication layers are in play on that hop, and they answer different
questions:

- **The service JWT** (`TERMINUS_PRIMARY_CHORD_JWT_SECRET`, shared with
  Chord's own `CHORD_JWT_SECRET`) authenticates *`terminus-primary` itself*
  to Chord — a fixed `{"sub": "lumina"}` claim Chord's relay requires,
  provisioned by the operator running `terminus-primary`, not by you.
- **The original caller's identity** — extracted from *their* mTLS client
  cert when they reached `terminus-primary`'s own front door — is forwarded
  alongside that JWT as a plain `X-Terminus-Client-Identity` header, purely
  for audit: it's additive metadata, not a second gate. Your tool
  implementation and Chord's own audit log can both see who actually asked,
  distinct from the service-to-service credential that authorized the hop
  itself.

This means you never provision `TERMINUS_PRIMARY_CHORD_JWT_SECRET` yourself —
that's the primary operator's secret, matched to Chord's. What *you*
provision is your own deployment's enrollment secrets (below) and its own
<secret-manager> bootstrap.

## Your own vault bootstrap — a separate identity from the fleet's

Per the fleet's per-service <secret-manager> convention, `terminus_personal` fetches
its own downstream secrets fresh from <secret-manager> at **every startup**, using
its **own** bootstrap identity — it never brokers secrets through another
service or reuses the fleet's own credential. Configure:

| Env var | Required | Default | Meaning |
|---|---|---|---|
| `INFISICAL_URL` | to enable the fetch | — | Your <secret-manager> instance's base URL. |
| `INFISICAL_CLIENT_ID` | to enable the fetch | — | Universal Auth client ID for *this deployment's own* <secret-manager> identity. |
| `INFISICAL_CLIENT_SECRET` | to enable the fetch | — | Matching client secret. Materialize this into the process environment at deploy time — never a literal in a script or unit file. |
| `TERMINUS_PERSONAL_INFISICAL_PROJECT_ID` | to enable the fetch | — | The <secret-manager> project/workspace ID to fetch from. |
| `TERMINUS_PERSONAL_INFISICAL_ENVIRONMENT` | no | `prod` | <secret-manager> environment slug. |
| `TERMINUS_PERSONAL_INFISICAL_SECRET_PATH` | no | `/` | Folder path within that environment/project to fetch secrets from. |

If `INFISICAL_URL`/`INFISICAL_CLIENT_ID`/`INFISICAL_CLIENT_SECRET` or the
project ID aren't all present, the fetch is skipped entirely (not attempted)
and the process falls back to whatever is already in its static environment
(e.g. a `.env` loaded by `EnvironmentFile=` in a systemd unit). A fetch that
*is* attempted but fails (auth rejection, network error, malformed response)
falls back the same way. Neither path is ever a hard startup failure, and no
secret value is ever logged — only counts, and for keys the fetch didn't
find, key *names* (never values). See
[`src/bin/terminus_personal.rs`](../../src/bin/terminus_personal.rs)'s module
doc for the full rationale.

### The `DOWNSTREAM_SECRET_KEYS` allowlist model

The fetch pulls a **fixed, named allowlist** of keys — never "every secret
found at this path." That's a deliberate anti-leak property: if your
<secret-manager> project/path is ever shared with another service, that other
service's secrets can't accidentally end up materialized into your process's
environment just because they happened to live alongside yours. As of this
writing the fixed allowlist is:

```
PLANE_API_URL
PLANE_API_KEY
PLANE_WORKSPACE
GITEA_URL
GITHUB_TOKEN
TERMINUS_ENROLLMENT_SHARED_SECRET
TERMINUS_JWT_SIGNING_KEY
```

The last two are there so your `/enroll` endpoint and mTLS JWT signing
actually work after a fresh fetch — without both present in the process
environment, `/enroll` reports `503 not configured` even if you've
provisioned the values in <secret-manager>, because the enrollment handler checks
the process environment directly (see
[`src/pki/enroll.rs`](../../src/pki/enroll.rs)).

A key present at the fetch path but with a **blank value** is treated as
*missing*, never set — a blank value can never silently overwrite a valid
value already present from a static fallback `.env`.

### Dynamic PAT prefixes — named identities without a code change

On top of the fixed allowlist, any secret key at the fetch path whose name
starts with one of these prefixes is materialized too:

- `PLANE_PAT_*` — per-identity Plane tokens (e.g. `PLANE_PAT_CLAUDE`,
  `PLANE_PAT_HARMONY`, or any identity name you provision).
- `GITEA_PAT_*` — per-identity Gitea tokens, same pattern.

This is a genuine *prefix match*, not another fixed list — provisioning a
brand-new named identity in <secret-manager> makes it usable on the process's next
restart with **no code change** to this repository. The same blank-as-missing
and anti-leak rules apply: a blank PAT value is never set, and a key that
doesn't start with one of these two prefixes is never picked up by this path
regardless of what else lives alongside it.

## Deploy walkthrough

### Core config

| Env var | Default | Meaning |
|---|---|---|
| `TERMINUS_PERSONAL_PORT` | `8300` | Bind port for the plain HTTP+JWT `/mcp` listener. |
| `TERMINUS_PERSONAL_BIND` | `127.0.0.1` | Bind address for that listener. `/mcp` is unauthenticated unless `TERMINUS_PERSONAL_TOKEN` is set, so this stays loopback-only by default — rely on a reverse proxy or the mTLS listener below for wider reachability, not a wide bind here. |
| `TERMINUS_PERSONAL_TOKEN` | unset | If set, the plain `/mcp` listener requires `Authorization: Bearer <value>`. |
| `TERMINUS_MTLS_BIND` | `127.0.0.1` | Bind address for the mTLS listener. |
| `TERMINUS_MTLS_PORT` | `8301` | Bind port for the mTLS listener — deliberately one past the plain listener's default, never colliding with it. |
| `TERMINUS_MTLS_SERVER_IDENTITY` | `terminus-primary` | This deployment's mTLS server-cert CN/SAN (an operator-facing label only — plays no role in client authz). |
| `TERMINUS_ENROLLMENT_SHARED_SECRET_<IDENTITY>` | — | Per-identity enrollment secret for each identity you want able to enroll against *your* deployment (see [`docs/deploy/client.md`](client.md) for the client side of this). |
| `TERMINUS_JWT_SIGNING_KEY` | — | HS256 key your `/enroll` endpoint signs issued JWTs with. |

Individual tool modules (Plane, Gitea, GitHub, Ansible, network, ...) read
their own env vars directly, populated either statically or by the <secret-manager>
fetch above — this binary does no additional config wiring beyond what's
listed here.

### Build and run — foreground first

```sh
cargo build --release --bin terminus_personal
set -a; source personal.env; set +a
./target/release/terminus_personal
```

Watch the startup log for the <secret-manager> fetch outcome and the tool count:

```
terminus_personal: fetched 9 secrets (3 named PAT identities) from <secret-manager>
terminus_personal: 47 tools registered, binding 127.0.0.1:8300 (auth: none)
```

### Run as a service

Same shape as the client daemon's unit
([`docs/deploy/client.md`](client.md#5-run-it-as-a-service)) — a dedicated
user, secrets only via `EnvironmentFile=`, never inline in the unit:

```ini
# /etc/systemd/system/terminus-personal.service
[Unit]
Description=terminus_personal (personal-registry Terminus deployment)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=terminus-personal
Group=terminus-personal
EnvironmentFile=/etc/terminus-personal/personal.env
ExecStart=/usr/local/bin/terminus_personal
Restart=on-failure
RestartSec=5

NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=read-only

[Install]
WantedBy=multi-user.target
```

```sh
sudo mkdir -p /etc/terminus-personal
sudo cp personal.env /etc/terminus-personal/personal.env
sudo chmod 600 /etc/terminus-personal/personal.env
sudo systemctl daemon-reload
sudo systemctl enable --now terminus-personal
```

### Wire it into the fleet

Once your deployment is up and reachable, tell the `terminus-primary`
operator:

1. Its base URL (for `TERMINUS_PRIMARY_CHORD_URL` on Chord's side, or
   whatever config wires Chord's `/v1/personal/tools/*` relay to your host —
   this is provisioned on Chord's side, not yours).
2. Nothing else. You do not provision `TERMINUS_PRIMARY_CHORD_JWT_SECRET` —
   that's the service credential authenticating the *hop into* your
   deployment, owned by whoever runs `terminus-primary`/Chord.

From that point on, any fleet caller reaching `terminus-primary` sees your
personal tools aggregated into its `tools/list`, and calls to them federate
through to your process automatically.

### Reach it directly, too

Your deployment also serves its own `/mcp` + `/enroll` + mTLS listener
directly — nothing requires going through the federation hop. If *you*
(under your own enrolled identity) want to call your personal tools without
the fleet in the loop at all, point a `terminus-client-daemon` straight at
your deployment's plain/mTLS ports exactly as described in
[`docs/deploy/client.md`](client.md), using the identity/secret you
provisioned for yourself above.

## Troubleshooting

- **`/enroll` returns 503 "not configured".** `TERMINUS_ENROLLMENT_SHARED_SECRET`
  (or the per-identity variant) and/or `TERMINUS_JWT_SIGNING_KEY` aren't in
  the process environment — check the startup log's <secret-manager> fetch outcome
  for a `missing` entry naming either key, or set them directly in your
  static `.env`.
- **A newly-provisioned `PLANE_PAT_*`/`GITEA_PAT_*` identity isn't showing
  up.** Restart the process — the dynamic prefix fetch only runs at startup,
  not on a timer; there's no hot-reload.
- **Federated calls from `terminus-primary` fail with a federation error, but
  calling your deployment directly works fine.** The break is on the
  `terminus-primary` ⇄ Chord ⇄ your-deployment hop, not your process itself —
  check Chord's relay config and `TERMINUS_PRIMARY_CHORD_JWT_SECRET` matching
  on the primary/Chord side.
- **Secrets look stale after an <secret-manager>-side rotation.** Restart the
  process — the fetch happens once per startup, by design (so a rotation
  requires a restart to take effect, but never requires anyone to manually
  re-splice a `.env` file).

---

Back to the [documentation index](../README.md). See also:
[Client deployment](client.md) · [Networking](../networking/README.md).
