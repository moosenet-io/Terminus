[← MINT overview](README.md)

# The `mint` CLI

`mint` is the unified front door for the MINT model-intake harness — a single `clap`-derived
binary (`src/bin/mint.rs`) that dispatches into the exact same library entry points as five
older, still-supported standalone binaries: `intake_coder_sweep`, `intake_coder_case`,
`intake_coder_gaps`, `intake_assistant_sweep`, and `gpu_mode`. `mint` does not duplicate any
orchestration logic — every subcommand resolves configuration (a CLI flag, falling back to the
same environment variable the legacy binary reads) and hands the resolved values to the
library's `run()` function (`src/bin/mint.rs:1-12`). The legacy binaries remain first-class,
unchanged, running the identical code path; `mint` is an additional, more discoverable entry
point layered on top, not a replacement (`src/bin/mint.rs:7-12`).

```
mint <COMMAND>

Commands:
  sweep       Fleet-level profiling sweeps (coder | assistant)
  case        Ad hoc single/multi-case rerun of the v2 code suite
  gaps        Case-id gap audit for a model
  gpu         GPU-authority lock: status | acquire | release
  supervisor  Permanent jam-detect + auto-recover daemon: run | install | uninstall
  fetch-model Delegate a model re-pull/re-quantize to Chord's PullCoordinator
```

## Configuration precedence

Every subcommand that takes a value which also has a legacy env var follows one rule,
implemented by `resolved_string()` (`src/bin/mint.rs:186-188`): **an explicit CLI flag always
wins, even over a differently-set env var; omitting the flag falls back to whatever the
env-sourced resolver returns.** `env_opt()` (`src/bin/mint.rs:190-192`) reads the env var,
trims it, and treats a blank value as unset. This mirrors exactly how the legacy binaries
already behaved when reading their env vars directly, so switching a deployment from (say)
`intake_coder_sweep` to `mint sweep coder` with no flags at all is behavior-preserving.

One field gets special normalization: `--case-limit 0` (on `mint sweep coder`) is treated
identically to never setting `INTAKE_CODE_CASE_LIMIT` at all — i.e. "no limit" — not a literal
zero-case cap, and it still defers to the env var if one is set (`normalize_case_limit`, tested
at `src/bin/mint.rs:830-885`).

## Schema migration on startup

Before dispatching, `main()` decides whether the invoked subcommand needs the shared intake
Postgres schema migrated first, via `needs_schema_migrate()` (`src/bin/mint.rs:232-249`).
Every subcommand **except** `gpu ...`, `supervisor ...`, and `fetch-model` triggers an explicit,
synchronous `ensure_schema_migrated()` call (`src/bin/mint.rs:219-230`) that connects to the
pool and runs `schema::migrate()`. This is deliberately **in addition to**, not instead of,
each library `run()` function's own defensive `migrate()` call — so the legacy standalone
binaries keep working unmodified when invoked directly, without ever going through `mint`.
`migrate()` is idempotent and advisory-lock serialized, so calling it twice per process is
always safe (`src/bin/mint.rs:219-226`).

- `gpu status`/`acquire`/`release` manage the GPU-authority **file** lock only — no Postgres
  dependency at all.
- `supervisor` is a pure DB **observer** of tables the sweeps own; a startup migrate would
  wrongly make the daemon refuse to start on a momentarily-unreachable DB, so it retries its
  own connection per tick instead (see [durability.md](durability.md) for the daemon's
  reconnect/retry lifecycle).
- `fetch-model` only talks to Chord over HTTP; it never touches the shared intake schema.

## `mint sweep coder`

```
mint sweep coder [--langs <csv>] [--case-limit <n>] [--mem-config <tag>] [--remote <host:port|url>]
```

Runs the fleet-level coder/builder profiling sweep — the same suite `intake_coder_sweep`
launches, now callable through `mint` (`src/bin/mint.rs:117-135`, dispatch at
`src/bin/mint.rs:266-296`). See [coder-eval.md](coder-eval.md) for the full evaluation
methodology.

| Flag | Env fallback | Meaning |
|---|---|---|
| `--langs` | `INTAKE_CODE_LANGS` | Comma-separated corpus language narrowing (e.g. `rust,python`). Omitted/empty ⇒ all languages the model's purpose-routing selects. |
| `--case-limit` | `INTAKE_CODE_CASE_LIMIT` | Cap on cases per model (smoke/debug runs). `0` (or unset) means no limit. |
| `--mem-config` | `SWEEP_MEM_CONFIG` | A tag recorded on the run's rows identifying the memory/runtime configuration under test. |
| `--remote` | `MINT_REMOTE_OLLAMA_URL` | Redirect the default Ollama backend's inference to a remote host — see **Remote inference target** below. |

As of the S86 revision, `mint sweep coder` does **not** pre-acquire a whole-run GPU-authority
guard itself. Earlier, an outer guard held for the whole subcommand was harmless because a
same-holder nested acquire was an idempotent no-op; that stopped holding once
`coder_sweep::run` began acquiring/releasing the exclusive lock freshly **per `(model,
backend)` pass** instead of once for the entire run. An outer guard now would make the first
pass's acquire a no-op but then have its release torn out from under it by the first inner
per-pass release, and its one-shot/no-backoff acquire would fail the whole subcommand on any
transient refusal — defeating the bounded backoff `coder_sweep::run` now performs on every
pass, including the first (`src/bin/mint.rs:279-296`). See [gpu-authority.md](gpu-authority.md)
for the full acquire/backoff/fairness model.

## `mint sweep assistant`

```
mint sweep assistant [--remote <host:port|url>]
```

Runs the fleet-level assistant/personality profiling sweep — the S84 consolidated suite,
same code path as `intake_assistant_sweep` (`src/bin/mint.rs:136-143`, dispatch at
`src/bin/mint.rs:297-324`). See [assistant-eval.md](assistant-eval.md) for the full
methodology. On completion it prints a one-line summary to stderr: `N/M models profiled, K
acquisition-skipped` (`src/bin/mint.rs:304-317`) — the identical message the standalone
`intake_assistant_sweep` binary prints. Same S86 reasoning as the coder sweep: the runner now
acquires/releases the exclusive GPU lock per model with bounded backoff on every acquire, so
`mint` does not pre-acquire an outer guard here either.

## `mint case`

```
mint case [--model <id>] [--ids <csv>] [--backend gpu|cpu] [--langs <csv>] [--mem-config <tag>] [--remote <host:port|url>]
```

An ad hoc rerun of an explicit `(model, backend, case_ids)` slice against the v2 code
suite — a targeted gap-fill without re-running a model's whole suite (`src/bin/mint.rs:44-69`,
dispatch at `src/bin/mint.rs:327-355`).

| Flag | Env fallback |
|---|---|
| `--model` | `INTAKE_CASE_MODEL` |
| `--ids` | `INTAKE_CASE_IDS` (comma-separated case ids from the v2 corpus manifest's `id` field) |
| `--backend` | `INTAKE_CASE_BACKEND` — `"gpu"` (default) or `"cpu"` |
| `--langs` | falls through to `coder_case::langs_from_env()` |
| `--mem-config` | falls through to `coder_case::mem_config_from_env()` |
| `--remote` | `MINT_REMOTE_OLLAMA_URL` |

Unlike the two sweep subcommands, `mint case` **does** take an explicit whole-invocation
`ExclusiveGuard::acquire(GpuMode::Exclusive, coder_case::GPU_HOLDER)` before calling
`coder_case::run` (`src/bin/mint.rs:344-354`) — a single case rerun is short enough that a
whole-run guard is still the correct shape; if the acquire fails, `mint case` exits with
`FAILURE` and never invokes the runner.

## `mint gaps`

```
mint gaps [--model <id>] [--mem-config <tag>] [--langs <csv>]
```

Audits which v2 code-suite case ids a model is missing valid data for, under a given
`mem_config`, and prints the result as a ready-to-paste `--ids`/`INTAKE_CASE_IDS` value so the
gap can be closed with `mint case` without rerunning the model's entire suite
(`src/bin/mint.rs:70-81`, dispatch at `src/bin/mint.rs:357-368`; see the standalone binary's
doc comment at `src/bin/intake_coder_gaps.rs:1-21` for the exact audit-scoping semantics of
`SWEEP_MEM_CONFIG`: unset audits rows with `mem_config IS NULL` — the carveout baseline
convention — while `SWEEP_MEM_CONFIG=carveout` scopes to rows explicitly labeled `'carveout'`
after a relabel). `mint gaps` takes **no GPU guard at all**: `coder_gaps::run` is a read-only
audit against already-persisted rows, never runs inference, and never touches the
GPU-authority lock (`src/bin/mint.rs:358-360`).

## `mint gpu status | acquire | release`

```
mint gpu status
mint gpu acquire [--mode exclusive|shared] [--holder <name>]
mint gpu release [--holder <name>]
```

A thin CLI over `intake::gpu_authority` (`src/bin/mint.rs:82-180`, dispatch at
`src/bin/mint.rs:370-404`). The full state model — lock format, PID-aware self-heal, the Chord
HTTP coordination + TTL heartbeat, and the fairness/max-hold safety valve — is documented in
[gpu-authority.md](gpu-authority.md); this CLI is a direct, side-effect-transparent wrapper:

- `status` — point-in-time query, no side effects. Prints the current lock's holder/mode/pid
  and whether the pid is still alive, plus whether the exclusive-mode Ollama drop-in is present.
- `acquire` — defaults to `--mode exclusive --holder mint`; applies that mode's policy (stopping
  competing services, reconciling Ollama's runner config) and takes the lock. Fails if a
  *different*, still-alive holder already has it.
  This is also available identically as the standalone `gpu_mode acquire <mode> <holder>` (a
  positional-arg CLI over the same functions, `src/bin/gpu_mode.rs:51-73`) — `gpu_mode` predates
  `mint` and is intended for operator/tooling use outside an automated sweep, run at the same
  trust level as `intake_coder_sweep` (it shells out to `systemctl` directly).
- `release` — defaults to `--holder mint`; releases that holder's lock, restarting exactly the
  services *that acquire* stopped. Does not revert Ollama's runner config — use
  `gpu acquire --mode shared` for that.

## `mint supervisor run | install | uninstall`

```
mint supervisor run
mint supervisor install
mint supervisor uninstall
```

The MINT Phase 3 permanent sweep supervisor (`src/bin/mint.rs:87-113`, dispatch at
`src/bin/mint.rs:406-412`). `run` is what a systemd unit's `ExecStart` invokes — the long-running
jam-detect + auto-recover daemon; `install`/`uninstall` write/enable or disable/remove that
systemd unit (deploy-time operations, not exercised by the Phase-3 build itself). See
[durability.md](durability.md) for the daemon's detection heuristics and recovery ladder.

## `mint fetch-model`

```
mint fetch-model [--model <id>]
```

Delegates a model re-pull/re-quantize to Chord's `PullCoordinator` over
`POST /api/models/:name/pull` (`src/bin/mint.rs:92-100`, dispatch at
`src/bin/mint.rs:415-432`). `--model` falls back to `INTAKE_CASE_MODEL` — the same env var
`mint case`/`mint gaps` fall back to, since this is treated as an ad hoc operator invocation
rather than a fleet-sweep target. The outcome is reported and mapped to a process exit code by
`report_fetch_model_outcome()` (`src/bin/mint.rs:439-470`):

| `PullOutcome` variant | Exit code | Message |
|---|---|---|
| `Warmed` | `SUCCESS` | model is now warm (present locally) |
| `NotFound` | `FAILURE` | model not found: `<detail>` |
| `InsufficientDiskSpace` | `FAILURE` | `<detail>` |
| `Unauthorized` | `FAILURE` | Chord rejected the pull (401/403) — set `CHORD_JWT` to a valid token for this harness host |
| `Unreachable` | `FAILURE` | Chord control endpoint unreachable: `<detail>` |
| `Failed` | `FAILURE` | `<detail>` |

Every non-`Warmed` variant maps to `FAILURE` (verified by
`report_fetch_model_outcome_every_other_variant_is_failure`, `src/bin/mint.rs:704-717`).

## Remote inference target (`--remote` / `MINT_REMOTE_OLLAMA_URL`)

`mint sweep coder`, `mint sweep assistant`, and `mint case` all accept `--remote` to redirect
the *default* Ollama backend's inference calls to a different host, without moving the harness
process itself — the harness still runs (and still locks the GPU) on its normal host; only the
inference target moves (`src/bin/mint.rs:62-67`). Resolution and normalization
(`resolve_remote_url()` / `normalize_remote_url()`, `src/bin/mint.rs:194-217`):

1. `--remote` wins over `MINT_REMOTE_OLLAMA_URL` if both are set; a blank value from either
   source is treated as unset.
2. A bare `host:port` (no scheme) is normalized to `http://host:port`; an explicit
   `http://`/`https://` URL passes through unchanged.
3. A trailing slash is trimmed (matching `context::ollama_base`'s own convention).
4. The resolved override is installed process-globally via `infer::set_remote_ollama_url(...)`
   before the sweep/case runs (intake runs sequentially, so this is safe).

Models pinned to a non-default backend (via the model registry's own routing) keep their own
routing regardless of `--remote` — the override only affects the *default* Ollama backend.

## Legacy standalone binaries

Each remains a thin wrapper around the same library `run()`/`run_from_env()` function `mint`
calls, reading the identical env vars, so a systemd unit built around the standalone binary
requires no changes to keep working:

| Binary | Library entry point | Env-sourced config |
|---|---|---|
| `intake_coder_sweep` | `coder_sweep::run` | `INTAKE_DATABASE_URL`/`DATABASE_URL`, `INTAKE_STAGING_DIR`, `MODEL_REGISTRY_PATH`, `OLLAMA_URL`/`_BASE_URL`/`_CPU_URL`, `INTAKE_CORPUS_V2_DIR`, `INTAKE_CODE_LANGS`, `INTAKE_CODE_CASE_LIMIT`, `INTAKE_VRAM_CEILING_GB` |
| `intake_coder_case` | `coder_case::run_from_env` | `INTAKE_CASE_MODEL` (required), `INTAKE_CASE_IDS` (required), `INTAKE_CASE_BACKEND`, `INTAKE_CODE_LANGS`, `SWEEP_MEM_CONFIG`, plus the shared DB/corpus vars above |
| `intake_coder_gaps` | `coder_gaps::run_from_env` | `INTAKE_CASE_MODEL` (required), `SWEEP_MEM_CONFIG`, `INTAKE_CODE_LANGS`, `INTAKE_DATABASE_URL`, `INTAKE_CORPUS_V2_DIR` |
| `intake_assistant_sweep` | `assistant::runner::run` | `INTAKE_DATABASE_URL`/`DATABASE_URL`, `INTAKE_STAGING_DIR`, `MODEL_REGISTRY_PATH`, `OLLAMA_URL`/proxy base, `JUDGE_<CLAUDE\|GEMINI\|CODEX>_CLI`/`_MODEL` |
| `gpu_mode` | `gpu_authority::{status,acquire,release}` | none — purely CLI-argument-driven |

All five source files carry a doc comment enumerating this env surface precisely
(`src/bin/intake_coder_sweep.rs:10-24`, `src/bin/intake_coder_case.rs:8-21`,
`src/bin/intake_coder_gaps.rs:12-21`, `src/bin/intake_assistant_sweep.rs:10-18`). Each spawns a
multi-threaded Tokio runtime (`#[tokio::main(flavor = "multi_thread")]`) — the suites mix async
I/O with libraries that expect a multi-thread scheduler, and a current-thread runtime risks
deadlocking inner inference futures (`src/bin/intake_coder_sweep.rs:34-37`). Every binary
(including `mint`) calls `terminus_rs::intake::init_tracing()` as its first line — without it,
`tracing::info!`/`warn!`/`error!` calls throughout the intake sweeps are silently dropped
(no subscriber installed), which previously hid progress logs like "still waiting for the GPU"
even though the emitting code was correct (`src/intake/mod.rs:50-77`). `init_tracing()` writes
to stderr, honors `RUST_LOG` (default `info`), and is safe to call more than once per process.

## Resume / skip-with-reason

The coder sweep's defining 24-hour-run property, unchanged whether launched via `mint sweep
coder` or `intake_coder_sweep` directly: a reboot or disconnect **resumes**, never restarts.
Each completed `(model, backend)` pass is appended to the file checkpoint *after* its rows are
persisted, and a resumed run skips any pass already in the checkpoint. A model that hangs, is
unavailable, exceeds the VRAM ceiling, or errors is recorded as a skip-with-reason and the
sweep continues — one bad model never wedges the fleet (`src/bin/intake_coder_sweep.rs:25-30`).
See [durability.md](durability.md) for the checkpoint file format and crash-consistency
ordering.

## Worked example (generic)

```console
$ mint sweep coder --langs rust,python --case-limit 5 --mem-config carveout
mint: schema migrate ok
...
assistant sweep complete: 11/12 models profiled, 1 acquisition-skipped (scores persisted to the intake DB)

$ mint gaps --model qwen3-coder:30b --mem-config carveout
qwen3-coder:30b: missing 3 case(s) under mem_config=carveout
INTAKE_CASE_IDS=rust-014,python-022,bash-003

$ mint case --model qwen3-coder:30b --ids rust-014,python-022,bash-003 --mem-config carveout
mint: GPU acquired: holder=intake_coder_case mode=exclusive
...

$ mint gpu status
GPU lock: none held
ollama exclusive drop-in present: false

$ mint fetch-model --model qwen3-coder:30b
fetch-model: qwen3-coder:30b is now warm (present locally)
```

(Model names above are literal identifiers as they appear in this codebase's own tests, e.g.
`src/bin/mint.rs:579`, not references to any specific deployment.)
