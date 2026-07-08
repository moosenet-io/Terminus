//! GitHub mirror engine — clean work-dir derivative of internal `main`.
//!
//! The mirror engine maintains, per `mirror_ready` repo, a PII-swept derivative
//! of internal `main` that keeps its own linear git history and shares ancestry
//! with the public `moosenet-io/*` GitHub mirror. It is built in layers:
//!
//!   * [`sweep`] (GHMR-02) — the **mechanical** transform: given a source tree
//!     and a config-driven placeholder map, rewrite deterministically-fixable PII
//!     (private IPs, container IDs, internal paths/URLs, org/host terms) into
//!     placeholder tokens, and report the **residual** (non-mechanical) violations
//!     that need judgment cleaning (GHMR-05). Detection of what is still PII after
//!     the mechanical pass reuses GHMR-01's authoritative gate
//!     ([`crate::github::pii`]).
//!   * [`workdir`] (GHMR-03) — the **clean work-dir manager**: per `mirror_ready`
//!     repo, it maintains a PII-swept derivative of internal `main` with its OWN
//!     linear git history (the lineage bridge to the public mirror). Each run
//!     syncs internal `main`'s tree content in, runs the [`sweep`], commits the
//!     swept state, and — iff the gate reports 0 residual violations — tags it
//!     `mirror-approved/<internal-sha>`.
//!   * mirror subtools (GHMR-04) build on top.
//!
//! The mechanical rewrite writes ONLY into a provided work-dir copy — never the
//! source repo. Producing and syncing that copy is GHMR-03's ([`workdir`])
//! concern; the sweep here operates on whatever tree path it is handed.

pub mod sweep;
pub mod workdir;
