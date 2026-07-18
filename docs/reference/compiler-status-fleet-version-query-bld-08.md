## `compiler_status` — fleet version query (BLD-08)

`compiler_status` (`compiler/status.rs`) is the compiler's **read** surface: what version of
each module is *available* (blessed in the artifact store) versus *deployed* on each fleet
host. It answers the fleet-wide version question the fleet GUI (BLD-15) and the Harmony fleet
API (BLD-16, `harmony-server/src/fleet.rs`) ask.

```
compiler_status(module?, probe_hosts=true)
```

It aggregates three sources and returns one structured JSON payload:

1. **Store `current` pointers** — for each `(module, channel)`, the blessed sha the artifact
   store points at. It reads `${BUILD_DATASET_ROOT}/artifacts/<module>/<channel>/current`
   (the pointer `compiler_release`/BLD-07 flips — file *or* symlink form); when that pointer
   is absent (BLD-07 not yet applied) it **degrades gracefully** to the newest published sha
   in the channel. It also lists every available sha per channel (`available`).
2. **module × host deployed-sha matrix** — each configured deploy host's `.deployed_sha`
   marker (what the constellation-updater wrote), read over the **existing host-reach path**
   (ssh `BatchMode`, `ConnectTimeout`, `cat --` — *no new credentials*). The probe is
   **side-effect-free**: `StrictHostKeyChecking=no` + `UserKnownHostsFile=/dev/null` so a
   read-only status call never mutates `known_hosts`. The remote command always exits 0 at
   the shell level, so ssh's exit code reflects **only connectivity** — that is what lets the
   matrix report an *unreachable* host distinctly from a *missing marker*. Each cell carries
   `deployed_sha`, the store's `current_sha`, `channel`, `built_at`, and a derived `status`
   (`up_to_date` / `update_available` / `undeployed` / `unknown`). An **unreachable host**
   degrades that cell to `unknown` and a **missing marker** to `undeployed` — never an error.
3. **queue / in-flight** — the build-scheduler surface. Until the job queue (BLD-06) lands
   these are empty lists with a `note` (a stable shape, not an error).

Output keys (the exact superset the fleet API parses): `current` (`{module:{channel:sha}}`),
`available`, `matrix` (`[{module,host,deployed_sha,current_sha,channel,built_at,status}]`),
`hosts` (`[{host,health,source}]`), `queue`, `in_flight`, plus `modules`, `degraded`, and
`notes`. When the artifact store is unconfigured or a host is unreachable the call still
returns a partial payload with `degraded=true`, never a hard failure.

Config (all optional, no infra literals — S1): `COMPILER_DEPLOY_HOSTS`
(`;`-separated `label|ssh_target`), `COMPILER_DEPLOY_MARKER_TEMPLATE` (default
`/opt/{module}/.deployed_sha`), `COMPILER_MODULES` (`,`-separated allow-list; default is to
enumerate the store), and `COMPILER_DEPLOY_SSH_TIMEOUT_SECS`. It reads no secrets from the
environment (S7).

