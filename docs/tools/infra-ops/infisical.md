# <secret-manager>

[← Infra & Ops index](README.md) · [← tool index](../README.md)

Source: [`src/<secret-manager>/mod.rs`](../../../src/<secret-manager>/mod.rs)

The `<secret-manager>` module is a read-only client for <secret-manager>, the vault
backend that holds every secret used across the constellation (per the
project's secrets discipline: no credential is ever hardcoded — everything
is pulled from <secret-manager> at runtime). It ports a legacy Python
`infisical_tools.py` exactly (`src/<secret-manager>/mod.rs:1-9`) and authenticates
fresh on every call via <secret-manager>'s Universal Auth flow
(`clientId`/`clientSecret` → short-lived bearer token) — unlike the Python
original, which cached the token per-process, this implementation holds no
shared mutable auth state (`src/<secret-manager>/mod.rs:16-19`).

**All five tools are GUARDED** — every `execute()` calls the shared
[`approval::gate()`](approval.md) before any HTTP request is made
(`src/<secret-manager>/mod.rs:394-397,431-434,502-505,586-589,666-669`). See
[`ansible.md`](ansible.md#the-approval-gate-in-detail) for the full gate
mechanics; this page focuses on what's specific to <secret-manager>.

<img src="../../../assets/guarded-tool-gate.svg" alt="Guarded tool approval gate sequence shared by ansible, <secret-manager>, and approval" width="100%">

## Configuration

| Env var | Purpose |
| --- | --- |
| `INFISICAL_URL` | Base URL of the <secret-manager> server, e.g. `http://<<secret-manager>-host>:8080` |
| `INFISICAL_CLIENT_ID` | Machine-identity client ID (Universal Auth) |
| `INFISICAL_CLIENT_SECRET` | Machine-identity client secret (Universal Auth) |

If any of the three is missing, `register()` still registers all five tools
(so the schema is always discoverable), but every call returns
`ToolError::NotConfigured` for the missing piece
(`src/<secret-manager>/mod.rs:694-700`). `InfisicalConfig` is `pub` rather than
`pub(crate)` specifically because `src/bin/terminus_personal.rs` — a
separate binary crate — needs it for startup-time secret bootstrapping
(PSEC-02); this does **not** relax the approval-gate requirement, which
stays exclusively on the MCP tool surface (`src/<secret-manager>/mod.rs:36-42`).

## Secret-value handling

Values are returned to the MCP caller exactly as <secret-manager> returns them
(this is the tool's job — an operator or agent asked for a secret value),
but this module itself **never logs or echoes a value** — every log
statement and every `_approval_code` gate summary names the *key*, never
the value (`src/<secret-manager>/mod.rs:582-585,663-665`). `infisical_list_secrets`
deliberately returns key names only; a unit test asserts the values never
leak into that response even if the backend includes them
(`src/<secret-manager>/mod.rs:808-827`).

## Tools

### `infisical_status`

**Purpose.** Check <secret-manager> server health and whether this identity can
authenticate.

**Input schema.** No parameters.

**Behavior.** GET `{base}/api/status` (no auth), then attempt
`get_access_token` — success maps to `auth: true`, failure maps to
`auth: <error string>`. Returns `{"server": <health body>, "auth": true|<error>}`.
Never itself throws on an auth failure — the auth attempt result is folded
into the response body, not propagated as an `Err`.

### `infisical_list_projects`

**Purpose.** List every project (workspace) the configured identity can see.

**Input schema.** No parameters.

**Behavior.** Authenticates, GETs `/api/v2/organizations/me/workspaces`,
and reshapes the response via `shape_projects` (`src/<secret-manager>/mod.rs:136-153`),
which accepts either a `{"workspaces": [...]}` wrapper or a bare array.

**Output shape:**
```json
{"projects": [{"id": "p1", "name": "Alpha", "slug": "alpha"}, {"id": "p2", "name": "Beta", "slug": ""}]}
```
Missing `slug` defaults to an empty string, never omitted.

### `infisical_list_secrets`

**Purpose.** List secret **keys only** (no values) in a project/environment/path.

**Input schema** (`src/<secret-manager>/mod.rs:468-478`)

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `project_id` | string | yes | — |
| `environment` | string | no | `"prod"` |
| `secret_path` | string | no | `"/"` |

**Behavior.** Gate runs before argument validation (a call with an empty
`project_id` still hits the gate first — verified by
`list_secrets_blocked_by_gate_without_db`, `src/<secret-manager>/mod.rs:912-918`).
After the gate, empty `project_id` is `InvalidArgument`. On success, GETs
`/api/v3/secrets/raw` with `workspaceId`/`environment`/`secretPath` query
params and shapes via `shape_list_secrets`
(`src/<secret-manager>/mod.rs:156-172`) — only `secretKey` is extracted from each
returned secret object; `secretValue` is dropped entirely before the
response is built.

**Output shape:**
```json
{"environment": "prod", "path": "/", "count": 2, "keys": ["FOO", "BAR"]}
```

### `infisical_get_secret`

**Purpose.** Retrieve one secret's actual value by key.

**Input schema** (`src/<secret-manager>/mod.rs:544-555`)

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `project_id` | string | yes | — |
| `secret_key` | string | yes | — |
| `environment` | string | no | `"prod"` |
| `secret_path` | string | no | `"/"` |

**Behavior.** Gate, then requires both `project_id` and `secret_key`
non-empty. The key is percent-encoded for the URL path
(`encode_key`, `src/<secret-manager>/mod.rs:268-279` — equivalent to Python's
`urllib.parse.quote(key, safe="")`; every byte outside
`[A-Za-z0-9._~-]` becomes `%XX`) and used in
`GET /api/v3/secrets/raw/{encoded_key}` with the same three query params.
Shaped via `shape_get_secret` (`src/<secret-manager>/mod.rs:175-183`).

**Output shape:**
```json
{"key": "API_KEY", "value": "the-actual-secret-value", "environment": "prod", "version": 4}
```
If the backend's response is missing the `secret` object, `key` falls back
to the requested key, `value` to `""`, `version` to `0` — never a panic on
a malformed body.

**Auth/guard note.** The approval-gate summary text names the key being
fetched but never its value: `"<secret-manager>: retrieve secret VALUE for key
'<key>' in project '<id>' env '<env>' path '<path>'"`
(`src/<secret-manager>/mod.rs:583-585`) — so even the pending-approval message the
operator sees in chat contains no secret material.

### `infisical_get_secrets_batch`

**Purpose.** Retrieve **all** secrets (keys + values) in a
project/environment/path in one call — for bulk injection, not browsing.

**Input schema** (`src/<secret-manager>/mod.rs:632-641`)

| Field | Type | Required | Default |
| --- | --- | --- | --- |
| `project_id` | string | yes | — |
| `environment` | string | no | `"prod"` |
| `secret_path` | string | no | `"/"` |

**Behavior.** Gate, then requires non-empty `project_id`. Delegates the
actual HTTP/auth fetch to `fetch_secrets_raw`
(`src/<secret-manager>/mod.rs:291-309`) — the **same** internal function
`fetch_secrets_batch` (below) builds on, so the auth+HTTP logic exists in
exactly one place. Unlike `fetch_secrets_batch`, this tool preserves its
pre-refactor behavior byte-for-byte: an <secret-manager>-side error (non-2xx) is
passed straight through as an `Ok` response body shaped
`{"error": true, "status": ..., "message": ...}`, never converted to a Rust
`Err` (`src/<secret-manager>/mod.rs:675-688`).

**Output shape:**
```json
{"environment": "prod", "path": "/", "count": 2, "secrets": {"A": "1", "B": "2"}}
```

### Internal: `fetch_secrets_batch` (not an MCP tool)

`pub async fn fetch_secrets_batch(config, project_id, environment,
secret_path) -> Result<HashMap<String, String>, ToolError>`
(`src/<secret-manager>/mod.rs:327-356`) is the reusable core also called directly
by `terminus_personal`'s own startup-time secret bootstrap (PSEC-02) — a
process-internal action, not an operator-invoked MCP call, so it has **no**
approval gate of its own (the gate stays exclusively on the guarded tool
surface). Unlike the guarded tool, a non-2xx <secret-manager> response here becomes
a typed `Err(ToolError::Http(...))` so the startup bootstrap gets a clean
pass/fail signal to decide whether to fall back to the static environment.
This function never logs or echoes a fetched value; callers must uphold the
same discipline.

## Security model summary

- All five MCP-facing tools are GUARDED.
- Read-only — there is no <secret-manager> write path anywhere in this module.
- Secret values only ever surface for `infisical_get_secret` and
  `infisical_get_secrets_batch`; `infisical_list_secrets` and
  `infisical_list_projects` are value-free by construction.
- No secret value is ever logged, and gate summaries name keys, never values.
- Fresh authentication per call — no shared token cache to leak or go stale
  incorrectly.

[← Infra & Ops index](README.md) · [← tool index](../README.md)
