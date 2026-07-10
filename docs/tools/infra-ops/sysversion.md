# sysversion

[← Infra & Ops index](README.md) · [← tool index](../README.md)

Source: [`src/sysversion/mod.rs`](../../../src/sysversion/mod.rs)

`sysversion` is a single tool, `system_version`, with an explicit
never-fail contract: it reports the version and reachability of every
component in the Lumina Constellation, and one down service must never fail
the whole report (`src/sysversion/mod.rs:1-12`).

<img src="../../../assets/readonly-http-probe-flow.svg" alt="Each of nine services is probed independently with a hard 2s timeout; an unreachable service reports status instead of erroring the whole tool" width="100%">

## Design contract

- **Never errors the whole tool** because one probe failed — each service
  gets its own independent probe with a hard 2-second timeout
  (`PROBE_TIMEOUT_SECS`); an unreachable service reports
  `{"status": "unreachable"}`, never propagates as a `ToolError`.
- **60-second process-wide cache** (`CACHE_TTL_SECS`) via a `OnceLock<Mutex<...>>`
  so repeated calls don't hammer every configured endpoint.
- **No hardcoded hosts/tokens** — every URL comes from an env var already
  used by another Terminus module; unset URLs report `"not_configured"`
  cleanly rather than guessing a default.
- **`secrets_backend` (<secret-manager>) never exposes a version** — reachable/
  unreachable only, by design (`src/sysversion/mod.rs:12,173-185`).

## Configuration (all reused from other modules — no new env vars)

| Service | Env var(s) | Fallback order |
| --- | --- | --- |
| `matrix_homeserver` | `MATRIX_HOMESERVER` | — |
| `model_server` (Ollama) | `OLLAMA_URL` → `OLLAMA_BASE_URL` → `OLLAMA_CPU_URL` | first set wins (`src/sysversion/mod.rs:138-142`) |
| `llm_proxy` (LiteLLM) | `LITELLM_URL` | — |
| `secrets_backend` (<secret-manager>) | `INFISICAL_URL` | — |
| `work_queue` (Plane) | `PLANE_API_URL` | — |
| `git_server` (Gitea) | `GITEA_URL` | — |
| `metrics_collector` (Prometheus) | `PROMETHEUS_URL` | — |
| `dgem_daemon` | `DGEM_BASE_URL` \| `DGEM_BIND`+`DGEM_HTTP_PORT` (default `127.0.0.1:8877`) | — |
| `chord_proxy` | `CHORD_PROXY_URL` | no compiled-in loopback fallback (2026-07 PII remediation) — unset means `not_configured` |
| `inference` | `CHORD_CONTROL_URL`, else derived from `CHORD_PROXY_URL` host + `CHORD_CONTROL_PORT` (default `8090`) | no fallback host either — `chord_control_base()` returns `None` when `CHORD_PROXY_URL` is also unset |
| `lumina_core` (in the `constellation` block) | `LUMINA_HTTP_URL` | optional — reports `"no_http_endpoint"` if unset |

## The tool: `system_version`

**Input schema.** No parameters (`additionalProperties: false`).

**Behavior.** All nine service probes run concurrently via `tokio::join!`
(`src/sysversion/mod.rs:401-426`), then the `constellation` block is built
from the already-probed Chord result plus a compiled-in `terminus_rs`
version and a best-effort `lumina_core` probe. Results are pretty-printed
JSON and cached for 60 seconds — a cache hit skips all network I/O entirely.

**Output shape:**
```json
{
  "constellation": {
    "lumina_core": {"version": "...", "status": "reachable"},
    "chord_proxy": {"version": "1.4.0", "commit": "...", "status": "reachable"},
    "terminus_rs": {"version": "0.x.y"}
  },
  "services": {
    "matrix_homeserver": {"status": "reachable", "server": "...", "version": "..."},
    "model_server": {"status": "reachable", "version": "0.5.x"},
    "llm_proxy": {"status": "reachable"},
    "secrets_backend": {"status": "reachable"},
    "work_queue": {"status": "reachable"},
    "git_server": {"status": "reachable", "version": "1.x"},
    "metrics_collector": {"status": "reachable"},
    "dgem_daemon": {"status": "reachable", "daemon": {"...": "..."}},
    "chord_proxy": {"status": "reachable", "version": "1.4.0", "commit": "...", "terminus_rs": "..."}
  },
  "inference": {
    "status": "reachable",
    "hot_model": "qwen3-coder:30b",
    "warm_models": [],
    "vram": {"...": "..."},
    "model_count": 4
  }
}
```

### Per-service probe behavior

- **`probe_matrix`** — tries `{base}/_matrix/federation/v1/version` first
  (extracts `server.name`/`server.version`); if federation is closed, falls
  back to a plain reachability check against `/_matrix/client/versions`.
- **`probe_ollama`** — GETs `/api/version`, extracts `version`.
- **`probe_litellm`** — reachability only, tries `/health/liveliness` then
  `/health`; reports `"configured"` (not `"reachable"`) if the endpoint
  itself is unauthenticated/unavailable but the base URL is set.
- **`probe_infisical`** — reachability only via `/api/status`, falling back
  to the bare base URL — **never returns a version field**, by design.
- **`probe_plane`** — reachability only, pings the bare API root.
- **`probe_gitea`** — GETs `/api/v1/version`, extracts `version`.
- **`probe_prometheus`** — reachability only via `/-/healthy`, falling back
  to the bare base URL.
- **`probe_dgem`** — GETs `/status` on the resolved DGEM base and nests the
  full daemon status object under `"daemon"`.
- **`probe_chord`** — GETs `/health`, extracts `version`, `commit`,
  `terminus_rs` — this is the *only* probe whose result is reused elsewhere
  (feeding the `constellation.chord_proxy` block).
- **`probe_inference`** — GETs `{chord_control_base}/api/models`; defensively
  parses a `{"models": [...], "count": N}` shape, deriving `hot_model`
  (first model with state `hot`/`loaded`/`active`), `warm_models` (state
  `warm`), and `vram` from either a top-level `vram` field or the first
  model record that has one. Malformed/missing fields never panic — they
  just leave the corresponding output field `null`/empty.

**Errors.** None — `execute()` cannot return `Err` under the design
contract; a test (`execute_never_errors_even_with_no_services`,
`src/sysversion/mod.rs:606-633`) clears every service env var and confirms
the tool still returns valid JSON with every top-level key present.

## Security model summary

- No auth tokens are sent by any probe in this module — every target is
  either LAN-trusted or (for <secret-manager>) probed for reachability only.
- `secrets_backend` never reports a version, deliberately, to avoid
  fingerprinting the <secret-manager> deployment via this tool.
- 2-second per-probe timeout bounds worst-case latency to roughly 2 seconds
  total (probes run concurrently), even with every service down.

[← Infra & Ops index](README.md) · [← tool index](../README.md)
