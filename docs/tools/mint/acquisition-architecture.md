# Model acquisition architecture — the harness directs Chord (ACQ-03)

The rule, stated once so no future change reintroduces an internet pull or lets the harness take
over Chord's role:

> **Chord owns model movement, memory loading, and the manifest/registry. The MINT harness
> DIRECTS and READS Chord — it never moves models itself, and never downloads from the
> internet/HuggingFace during a sweep.**

## Responsibility split

| Concern | Owner | Where |
|---|---|---|
| Which models exist, their tier (hot/warm/cold), sizes, paths, timestamps | **Chord** | `src/models/registry.rs` (`ModelRecord`, `RegistryFile`) |
| Moving a model cold→hot (promotion from the tiered archive) | **Chord** | `src/models/transfer.rs` (`PullCoordinator::ensure_local`) |
| Hot-tier eviction / disk pressure | **Chord** | `src/models/eviction.rs`, `gc.rs` |
| Deciding *which capabilities* a model can serve and testing them on this hardware | **the harness** | `src/intake/` |

The harness's job is to figure out which Chord settings/providers can serve a model and how,
then measure the model's capabilities on this hardware to build the profiles — not to
reimplement anything Chord owns.

## The acquisition path (per swept model)

1. Before serving/testing a model, the sweep calls **`chord_pull::acquire_via_chord(model)`**
   (`src/intake/chord_pull.rs`), which issues `POST {CHORD_CONTROL_URL}/api/models/{name}/pull`
   (Bearer-JWT via `CHORD_JWT`).
2. Chord's `ensure_local` promotes the model from **cold/warm tiered storage** (the QNAP archive)
   to the local hot tier — a local/NFS copy, **never a network fetch**. Concurrent pulls of the
   same model are de-duped server-side behind a per-model lock.
3. `PullOutcome::Warmed` ⇒ proceed to serve/test. A hard failure maps to a typed non-viable
   outcome and a **non-viable row** (survivorship-bias fix), and the sweep continues to the next
   case — no batch crash:
   - `404` (unknown model, or known but missing from the archive) → `failure_class` unavailable
   - `507` (insufficient local disk) → `failure_class` resource
   - `NotConfigured` (`CHORD_CONTROL_URL`/`CHORD_JWT` unset) → non-viable, logged once — **never
     guess a host, never fall back to the internet**

Both sweep kinds acquire through this one unified hook.

## Hard rule: no internet pulls from the harness, ever

The sweep has **no** `ollama pull`, HuggingFace fetch, or other internet acquisition path
(ACQ-01 removed the last shell-outs). Its only remote call for acquisition is Chord's control
endpoint. If a model is not on the cold archive, the sweep records a non-viable row and moves on
— it does **not** reach out to the internet to get it. Populating the cold archive with *new*
candidates is a separate, deliberate ingestion step (the ASMT-08/09 discovery/acquisition path),
not something a sweep does mid-run.

## How this composes with the rest

- **GPU-exclusive serving** ([`gpu-authority.md`](gpu-authority.md)): acquisition (making the
  model resident on disk via Chord) is distinct from holding the GPU to serve it. The existing
  acquire→serve sequencing and the GPU-exclusive lease are unchanged.
- **Restart resilience** ([`restart-resilience.md`](restart-resilience.md)): a model that Chord
  has to promote from cold storage on resume is handled the same way as on first run — the
  harness just directs Chord to `ensure_local` again (idempotent).

## Configuration (env var **names** only)

- `CHORD_CONTROL_URL` — Chord's control API base URL (same var `serving_tools` already uses).
- `CHORD_JWT` — the Bearer token, read via the same convention as `gpu_authority::chord_auth_token`
  (trimmed; empty ⇒ no token, matching Chord's own auth-disabled mode). Never a raw hardcoded value.
