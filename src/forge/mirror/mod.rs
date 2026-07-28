//! git-public mirror engine — clean work-dir derivative of internal `main`.
//!
//! (Renamed at GITX-08 from the GitHub-specific `github::mirror`; the engine
//! has been behaviorally provider-agnostic since GITX-05's
//! `dispatch_mirror_action` / `mirror_provider_token()` routing — GitHub
//! remains the only currently-configured mirror target.)
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
//!   * [`clean`] (GHMR-05) — the **operationalized residual-cleaning pass**: when
//!     the sweep leaves residual (non-mechanical) violations, a bounded (≤3 rounds)
//!     loop dispatches a scoped cleaning subagent that remediates the flagged spots
//!     IN THE WORK DIR ONLY, re-runs the gate each round, and either drives the
//!     residuals to 0 (tag-able) or escalates the exact `file:line` spots to the
//!     operator. Wired into `git_public_mirror_prepare` (GHMR-04).
//!   * mirror subtools (GHMR-04) build on top.
//!   * [`runner`] (MRUN-01) — the missing scheduling piece: a single
//!     idempotent per-repo "run once" orchestration
//!     (status → backfill+gate → fast-forward sync/push) plus the
//!     `git_public_mirror_run` tool, driven by
//!     `deploy/terminus-mirror-runner.timer` so the public mirror keeps
//!     advancing without an operator remembering to run the GHIST tools by
//!     hand. Orchestration only — see the module doc for why it never
//!     duplicates git/PII/transport logic and never force-pushes.
//!
//! The mechanical rewrite writes ONLY into a provided work-dir copy — never the
//! source repo. Producing and syncing that copy is GHMR-03's ([`workdir`])
//! concern; the sweep here operates on whatever tree path it is handed.

pub mod clean;
pub(crate) mod discovery;
pub mod history;
pub mod native_clean;
pub mod pr_replay;
pub mod runner;
pub mod sweep;
pub mod tools;
pub mod workdir;

/// Process-wide monotonic counter, combined below with pid+nanos, to guarantee a
/// unique temp-path suffix for every mirror scratch dir (bare repos, work-dirs,
/// gate scan roots, askpass scripts, …) allocated anywhere in this module tree.
///
/// `pid + nanos` alone is unique in the overwhelming common case, but it has two
/// real failure modes under the compiler test-gate: (a) two allocations *in the
/// same process* landing in the same clock tick when the host's timer resolution
/// is coarser than expected, and (b) two *separate* gate build processes that
/// happen to share both a PID (containers/sandboxes often reuse low PIDs) and a
/// nearby wall-clock tick when they share a bind-mounted `/tmp`. The atomic
/// counter makes same-process collisions structurally impossible (every call in
/// a process gets a distinct sequence number), and folding it into the suffix
/// makes accidental cross-process reuse require BOTH a PID collision AND a
/// nanosecond-clock collision AND a sequence-number collision — vanishingly
/// unlikely rather than merely unlikely.
static UNIQUE_TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Build a filesystem-safe, process- and call-unique suffix `<pid>-<nanos>-<seq>`.
/// Callers prefix this with a stable tag identifying the call site, e.g.
/// `std::env::temp_dir().join(format!("ghmr04-bare-{}", unique_temp_suffix()))`.
pub(crate) fn unique_temp_suffix() -> String {
    let seq = UNIQUE_TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}-{}", std::process::id(), nanos, seq)
}
