## `compiler_progress` — live build progress/events (BLD-19)

`compiler_progress` is the **live progress surface** over `compiler_build`, so a client (the
fleet GUI, a requesting agent, the Harmony adapter) can render a real progress bar instead of
a spinner. Every `compiler_build` call carries a stable `request_id` (supply one, or read the
auto-generated id back from the build result / its `structured.request_id`), and each stage of
the build emits a typed event into a per-request ring buffer + a live broadcast channel.

The `request_id` is returned on **both** outcomes: on success in the result text +
`structured.request_id`; on a build **failure** it is prefixed into the returned error
(`[request_id=<id>] …`), so a failed build's progress stream (terminal `failed` event + the
redacted error tail) stays discoverable even when the caller did not supply an id up front.

A caller-supplied `request_id` must be a single `[A-Za-z0-9._-]` segment of at most 128 bytes.
This is a **hard validation rule, not a lossy clamp** — and it is validated **RAW, with no
trimming or normalization** (so `" build-1 "` and `"build-1"` are *distinct* values, neither
collapsed onto the other's track): a value with leading/trailing/inner whitespace (or any
disallowed char, or overlong) is **invalid**. `compiler_build` **falls back to an
auto-generated id** when the supplied one is missing or invalid (never returning without a
surfaced id), and `compiler_progress` **rejects** an invalid/overlong/whitespace-bearing id
with a clear validation error. When a supplied id is invalid and substituted, the fallback is
**observable, not silent**: a `tracing::warn`, a `supplied_request_id_invalid: true` field in
the success structured output, and a `[supplied_request_id_invalid]` marker in the returned
error on failure — so a client can correlate the id it sent with the effective id used.

```
compiler_progress(request_id, since=0, wait_ms=0)
```

### The event model

Each build progresses through ordered **stages**:

```
queued → scheduled → [relaying (remote only)] → building{step,total} → publishing → published | failed
```

Every **stage transition** is emitted and retained exactly once — even a build whose cargo
output has no parseable `{step,total}` line still shows a `building` (started) event. The
throttling below applies **only** to intermediate `{step,total}` progress updates, never to a
stage transition.

- `pending` — **snapshot-only, never an emitted event.** The track exists (it was
  `begin`-rotated) but no event has been emitted yet — the brief window between the wrapper's
  rotation and the build's first `queued`. Reported as `pending` (non-terminal) so a poller in
  that window never observes a *fabricated* `queued` that was never emitted.
- `queued` — request accepted (carries `module@ref`).
- `scheduled` — build host selected (`primary`/`heavy`).
- `relaying` — source staged (rsync) to the heavy host. **Remote/heavy path only** — a local
  (in-place) build has nothing to relay and legitimately goes `scheduled → building` directly;
  a local stream without a `relaying` event is valid, not a gap.
- `building` — compilation in progress. The started transition is always emitted; then
  `{step,total}` is parsed from cargo's build progress (`N/M`) and streamed live as the crates
  compile, throttled so an unchanged step is not re-emitted. To make this work on the build's
  **piped, non-TTY** stdio, the build child env forces `CARGO_TERM_PROGRESS_WHEN=always` (with a
  fixed `CARGO_TERM_PROGRESS_WIDTH`) so cargo renders the `N/M` bar, and the output drain splits
  on **both `\r` and `\n`** so each carriage-return progress update reaches the tap live (cargo
  redraws the bar with `\r`, not newlines) rather than buffering until the next newline.
- `publishing` — artifact being checksummed + written.
- `published` — **terminal success**, carries the artifact `sha256`.
- `failed` — **terminal failure**, carries a **secret-sanitized** error tail.

`published` and `failed` are the **only** terminal stages on this stream — once terminal, the
stream is **closed** and any later event is ignored. `compiler_build`'s scope ends at publish;
the downstream updater/deploy lifecycle (`deployed` / `rolled_back`, BLD-13) is a **separate**
concern that is **not** emitted onto this stream, so the code and docs agree that nothing
follows `published`/`failed` here.

Every event has a monotonic per-build `seq`, a timestamp (`ts_ms`), the `stage`, optional
`{step,total}`, an optional short `message`, and (on a terminal success) the `sha`.

### Query vs. subscribe (snapshot + long-poll)

- **Snapshot** (`wait_ms` omitted/0): returns the current stage, the latest `{step,total}`,
  timing, `last_seq`, and the events with `seq > since` (or the whole retained tail when
  `since = 0`) — so a *late* subscriber still gets the current state plus the recent tail.
- **Long-poll / subscribe** (`wait_ms > 0`, capped at 30s): blocks until the next event (or
  the timeout), then returns a fresh snapshot. To **stream**, pass `since` = the last `seq`
  you saw and advance it each call: `compiler_progress(id, since=last_seq, wait_ms=5000)`.

An unknown or expired `request_id` returns `{"status":"not_found"}` — never an error. Each
snapshot carries a per-build `generation` so reused ids stay isolated (tracks are
per-build-request, not per-key-slot):

- **A build beginning rotates the stream.** `compiler_build` calls `begin(request_id)` in the
  tool wrapper **before any validation** (the single rotation per attempt) — replacing any
  existing track for that id (live OR already-terminal) with a fresh one (new generation, empty
  ring, non-terminal). Reusing a still-tracked id therefore always shows a **clean per-build
  stream** — never a previous build's stale terminal state, and never dropping the new build's
  events into a closed terminal track. Because rotation precedes validation, even a
  **pre-acceptance failure** (invalid `module`/`ref`, before `queued`) lands its terminal
  `failed` on the fresh track, so a reused id's failure is never masked by a prior terminal
  build; that attempt is a discoverable terminal-only `failed` track (no synthesized `queued`).
- **Long-poll is generation-safe.** `subscribe` captures the receiver, the snapshot, and the
  generation from the **same track under one lock** (no TOCTOU). If that track is
  evicted/rotated and the id is reused while a long-poller is waiting, the waiter's post-wake
  snapshot has a different `generation` and resolves to `not_found` — a stale waiter never
  receives a different build's data.
- **Long-poll is TTL-bounded.** The wait is bounded by the earlier of `wait_ms` and the track's
  remaining TTL, so an id that expires *during* a long-poll wakes promptly (≈ at the TTL, not at
  the `wait_ms` cap) and resolves to `not_found` — it never hangs to the cap.

### Seam with `compiler_status` (BLD-08)

`compiler_status` is the **point-in-time** aggregate — the queue, the store `current`
pointers, and the module×host deployed-sha matrix (*what is deployed where, right now*).
`compiler_progress` is the **live per-request event stream** (*how is this build going,
second by second*). They do not overlap: use status for fleet deploy state, progress to
watch a specific build.

### Store, bounds & discipline

The event store is **in-process and ephemeral** (a ring buffer + broadcast channel per
build) — progress is transient, exactly like the BLD-20 admission queue; it fails open and
never blocks a build, and `compiler_status` remains the durable point-in-time truth if the
process restarts. The emit boundary is **panic-safe**: an unexpected panic in bus logic is
caught + logged and never propagates, so a bus hiccup can never abort the build it is only
reporting on. Three numeric, env-tunable bounds keep memory bounded:

| Env knob | Default | Meaning |
| --- | --- | --- |
| `COMPILER_PROGRESS_MAX_EVENTS` | `256` | Ring-buffer depth per build (oldest events fall off). |
| `COMPILER_PROGRESS_MAX_BUILDS` | `64` | Max tracked builds (least-recently-updated evicted at capacity). |
| `COMPILER_PROGRESS_TTL_SECS` | `3600` | Idle TTL; a build untouched this long is swept on the next write **and** enforced on every read, so a quiet process still returns `not_found` for an expired id. |

Log-tail lines are **secret-sanitized by the emitter** (`compiler_build` runs every captured
cargo output line through its existing S6/S7 redaction set) *before* they enter the bus, so a
secret never leaves the process through the stream. A **failed-event message** is sanitized in
two passes before it is persisted: secret **values** (S6/S7) then infrastructure **literals**
(S1) — IP addresses, the emitter-known configured host/relay-host and dataset/deploy path
values, and the sanctioned repo-wide S1/PII scanner as a catch-all — each replaced by a
placeholder (`<ip>`/`<host>`/`<path>`), so no configured path, internal host, or IP can leave
through the stream. The bus stores only stage/timing/step data and already-sanitized text — no
infrastructure literals (S1), no secrets (S6/S7).

A **pre-acceptance failure** (a validation/config error before the build emits `queued`)
yields a discoverable **terminal-only** `failed` track — the id is still surfaced and the
stream is queryable; no fake `queued` event is synthesized to pad the shape.

