## MINT idle-mode — release the host for a CI/CD compiler run (BLD-10)

The MINT harness runs GPU-heavy profiling sweeps on the same big host the constellation
CI/CD compiler (S117) needs for heavy builds. `crate::mint::idle` lets the compiler ask
MINT to go **idle** so that host's RAM/VRAM is freed on demand, mirroring
[Chord's BLD-09 idle-mode](https://github.com/moosenet-io/Chord) contract:

- **`enter_idle(reason)`** — stop admitting new sweep/case runs, drain what is in flight
  (closed-world), release MINT's own `gpu_authority` exclusive lock (handing the shared
  GPU back), and report the freed `MemAvailable` delta. Idempotent (already idle ⇒ no-op).
- **`activate(reason)`** — resume normal harness operation; the next sweep/case
  re-acquires the GPU lock lazily, exactly as from a cold start. Idempotent. Also happens
  automatically on the next admitted run (`admit_run`) unless a compiler build lease is
  still held — in which case idle is deliberately **preserved** so a stray run can't tear
  the build window down.
- A **watchdog** (`watchdog_loop`) re-activates on a timeout so MINT is never left
  silently idle, deferring only while a live **compiler** GPU lease is held (a non-compiler
  holder does not extend the window). Transient states are never persisted (a crash mid-
  transition reloads Active); the resume manifest persists only when a state path
  (`MINT_IDLE_STATE_PATH`) is configured.

Everything is config-driven (`MINT_IDLE_*`, `MINT_GPU_HOLDERS`) with no hardcoded infra
values, and the GPU/RAM side effects are best-effort. The compiler drives idle/activate
around a build via the lease wiring in BLD-11.

