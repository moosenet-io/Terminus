//! MINT — the model-intake profiling test-harness (`bin/mint`, the coder/
//! assistant sweeps, ad hoc case reruns, breakfix). This module hosts the
//! cross-cutting control surfaces the harness exposes to the rest of the
//! constellation.
//!
//! ## `idle` — BLD-10 (S117 constellation CI/CD compiler)
//! The shared big host runs both the LLM proxy (Chord) and MINT's GPU-heavy
//! sweeps. When the CI/CD compiler needs that host for a heavy build it asks
//! both to go *idle* so their VRAM/RAM is freed. MINT's idle-mode
//! ([`idle`]) is the exact hardened parallel of Chord's BLD-09 idle-mode: a
//! closed-world 4-phase state machine (Active → EnteringIdle → Idle →
//! Activating) with atomic CAS enter/activate, generation-guarded transitions,
//! RAII cancellation rollback, a release bounded strictly under the watchdog
//! stale threshold, compiler-lease-aware lazy restore + watchdog, transient
//! phases never persisted, and a hard-gated activate persist. See
//! [`idle`] for the full contract.

pub mod idle;
