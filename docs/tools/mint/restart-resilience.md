# MINT sweep restart resilience (RESIL-04)

How the model sweep survives a restart — of the sweep process (Terminus side) **and** of
Chord — without losing its place or falsely tripping "CHORD LOCK GAP DETECTED". This is the
end-to-end view; the per-component mechanics live in [`durability.md`](durability.md) and the
Chord repo.

## What survives what

| Failure | Covered by | Result |
|---|---|---|
| **Sweep process restart** (Terminus binary redeploy/crash) | file checkpoint (`checkpoint.rs`) **and** Chord session cache (RESIL-02/03) | resumes from the union of "done" signals; completed cases are never re-run |
| **Chord restart** (backbone redeploy/crash) mid-sweep | Chord GPU-exclusive lease persistence (RESIL-01) | the sweep's live GPU lease is reloaded on Chord startup; no gap, no competing job slips in |
| **Local checkpoint dir lost** (fresh box / wiped staging) | Chord session cache (RESIL-02/03) | resumes purely from Chord's remaining set |
| **Chord unreachable/unconfigured** at sweep start | file checkpoint | degrades cleanly to file-only resume; the run still proceeds |

## The three durable authorities

1. **Chord GPU-exclusive lease persistence (RESIL-01, Chord `src/gpu_exclusive.rs`).**
   The GPU-exclusive lock is persisted to `<CHORD_STATE_DIR>/gpu_exclusive_lease.json`
   (atomic tempfile+rename) on every acquire/heartbeat/release, and reloaded on Chord startup
   honoring the TTL (`CHORD_GPU_EXCLUSIVE_TTL_SECS`). A restart mid-sweep therefore keeps
   honoring the sweep's live lease instead of dropping it. An already-expired lease is **not**
   reloaded (an abandoned sweep must never relock the GPU). Best-effort: a missing/corrupt file
   never panics Chord; `CHORD_STATE_DIR` unset ⇒ in-memory-only (the prior behavior).

2. **Chord sweep-session cache (RESIL-02, Chord `src/sweep_session.rs` + control routes).**
   Chord durably records a session's planned **action queue** + progress cursor
   (`<CHORD_STATE_DIR>/sweep_sessions.json`), served by three JWT-gated endpoints:
   `POST /api/sweep/session` (idempotent register/upsert), `GET /api/sweep/session/:id`
   (remaining in queue order + counts), `POST /api/sweep/session/:id/advance` (mark keys done).
   Chord only **records and serves** the queue — it never executes it.

3. **File checkpoint (`checkpoint.rs`, pre-existing).** The fast local resume path: a
   JSON-lines, append-on-`mark` set of completed-work keys on the NAS staging dir. Still the
   first line of defense and the source of truth Chord's cursor is reconciled against.

## Resume decision (Terminus side, RESIL-03)

At sweep start the coder sweep (`coder_sweep.rs` via `chord_session.rs`):

1. Builds the planned queue from the (possibly `--only-stale`-narrowed) fleet and derives a
   **stable `session_id`** = `mint-<run_kind>-<epoch>-<sha1(queue)>`. Same queue on restart ⇒
   same id (so the sweep finds its own session); a materially different fleet/selection ⇒ a new
   id, matching Chord's replace-on-different-queue semantics.
2. `register`s the queue with Chord (best-effort — an unconfigured/unreachable Chord is logged
   once and the run proceeds file-only; **never fatal, never an internet call**).
3. **Reconciles**: any unit Chord already reports done backfills the file checkpoint, and any
   unit the file marks done is treated as done — a case marked complete by *either* source is
   never re-run. So a Terminus restart resumes from Chord even if the local file dir was lost,
   and from the file if Chord was reset.
4. As each case's rows land in Postgres and the file checkpoint is `mark`ed, it `advance`s
   Chord's cursor (best-effort, after the mark — a failure is logged, never fatal).

`ActionKey` = `<run_kind>|<model>|<backend>[|<case>]`, the same conceptual unit the file
checkpoint keys on, correlated 1:1 so no string round-trip is needed to reconcile the two.

## Failure modes / degradation

- **Chord down** → register/advance are no-ops (logged once); file checkpoint drives resume.
- **`CHORD_STATE_DIR` unset on Chord** → lease + session cache are in-memory only (lost on
  Chord restart); the sweep still resumes from its file checkpoint. Set `CHORD_STATE_DIR` on the
  Chord host to get the cross-Chord-restart guarantee.
- **Both Chord-remaining and file-done empty** → a genuinely fresh sweep; plan from scratch.

## Configuration (env var **names** only)

- Chord host: `CHORD_STATE_DIR` (enables RESIL-01/02 persistence), `CHORD_GPU_EXCLUSIVE_TTL_SECS`.
- Sweep host: `CHORD_CONTROL_URL`, `CHORD_JWT` (the typed session client, same convention as
  `chord_pull.rs`), `INTAKE_STAGING_DIR` (file checkpoint), `MINT_SWEEP_SESSION_TIMEOUT_SECS`.

## Known follow-up

The assistant sweep (`assistant/runner.rs`) currently resumes via its file checkpoint only; the
generic `chord_session` primitive is ready to wire into its per-dimension loop (RESIL-03b
follow-up). The coder sweep — the longer, GPU-heavy run — has the full Chord-backed resume.
