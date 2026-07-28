//! MRUN-01 — mirror-runner orchestration + `git_public_mirror_run` tool.
//!
//! S115 audit finding: `TERMINUS_MIRROR_AUTO_APPROVE=true` was set on the
//! assumption that *something* drove the git-public mirror on a schedule, but
//! no mirror-runner/timer ever existed — nothing ever called
//! [`crate::forge::mirror::tools`]'s `git_public_history_*` tools
//! periodically. The public mirror silently stopped advancing until it
//! diverged/fell behind and an operator had to notice and intervene by hand.
//! This module is the missing piece: a single idempotent "run once" pass over
//! one repo ([`run_once`]), wired up per-repo by
//! `deploy/terminus-mirror-runner.{service,timer}`.
//!
//! ## Orchestration only — no new git/PII logic
//! [`run_once`] does not touch git, PII scanning, or credentials itself. It
//! calls the SAME `git_public_history_status` / `_backfill` / `_sync` tools an
//! operator would call by hand (via the thin `pub(crate)` wrappers in
//! [`super::tools`]), and classifies their JSON responses into one of four
//! outcomes. Every safety property (fast-forward-only push, the full-history
//! PII hard-block, `TERMINUS_MIRROR_AUTHOR_MAP` fail-closed, never-force
//! transport) is enforced by that existing code, not reimplemented here.
//!
//! ## Never forces (load-bearing)
//! `git_public_history_sync` NEVER force-pushes — it fast-forwards an already
//! operator-blessed baseline (GHIST-07) and REFUSES (a [`ToolError::Conflict`])
//! on any non-fast-forward / diverged / not-yet-bootstrapped condition. This
//! module maps every such refusal to [`RunOutcome::NeedsOperatorRebaseline`]
//! and returns — it never retries with a different tool, a different arg
//! shape, or any `--force`/`-f` git flag. The one sanctioned force is the
//! operator-blessed GHIST-07 bootstrap re-baseline, performed by a human
//! outside this module entirely. See `run_once_never_calls_sync_after_conflict`
//! and the module's grep-based negative test in the test module below.
//!
//! ## Source-sync is a separate, dev-box-side prerequisite (load-bearing)
//! The host that runs this runner (wherever the mirror engine's work dirs and
//! git-transport credentials live — "terminus-primary" in deploy config) is
//! expected to mount the `TERMINUS_MIRROR_SOURCE_ROOT` parking lot **READ-ONLY**.
//! Keeping the parking lot's internal-`main` checkout current (`git fetch` +
//! `checkout` + `reset --hard origin/<branch>`) is `git_public_mirror_sync_source`'s
//! job (GHMR-04/MIRR-04), run from the dev box that holds the Gitea
//! credential — NOT from here. `run_once` assumes the parking lot is already
//! current for `repo` and only mirrors what it finds there; a runner that
//! discovers `commits_behind` staying nonzero across ticks should be
//! investigated as a source-sync gap, not "fixed" by teaching this module to
//! write into the parking lot.
//!
//! ## MIRROR-AUTO (opt-out discovery + auto-baseline, added on top of MRUN-01)
//! Two additions, both narrowly scoped so the "never forces" contract above
//! stays true for every OTHER path in this module:
//!
//!   1. **Discovery flips from opt-in to opt-out.** The old
//!      `discover_mirror_ready_repos` required an explicit
//!      `mirror_ready: true` in each repo's `.moosenet-pipeline.yaml`. Now
//!      [`list_mirror_candidates`] treats every repo under
//!      `TERMINUS_MIRROR_SOURCE_ROOT` as a CANDIDATE unless blacklisted
//!      (`TERMINUS_MIRROR_BLACKLIST`) or explicitly `mirror_ready: false`,
//!      and [`resolve_and_verify_remote`] only actually mirrors a candidate
//!      once [`super::discovery::discover_public_remote`] confirms a real
//!      public GitHub repo already exists for it (name-mapped/org-configured
//!      via `TERMINUS_MIRROR_GITHUB_ORG`/`TERMINUS_MIRROR_NAME_MAP`) — that
//!      existence check IS the opt-out. An operator-supplied override remote
//!      is subjected to the SAME existence/org check
//!      ([`super::discovery::verify_public_remote`]) so it can never redirect
//!      an auto-push at an unverified target (codex security fix).
//!   2. **Auto-baseline for the safe first-time case.** [`run_once_with`]'s
//!      no-lineage branch, when `cfg.auto_baseline` is set (production
//!      default: `TERMINUS_MIRROR_AUTO_BASELINE`, default TRUE), runs the
//!      SAME full-history backfill + full-history PII gate a human GHIST-07
//!      bootstrap would require, and — ONLY on a gate-clean result — calls
//!      [`super::tools::bootstrap_first_push`], which pushes ONLY when the
//!      remote genuinely has no `main` branch yet (nothing to overwrite, so
//!      no `--force` is ever needed) and refuses (mapped to
//!      `NeedsOperatorRebaseline`) if the remote unexpectedly already has
//!      content. Residual PII withholds the push unconditionally on this
//!      path too — see `auto_baseline_gate_dirty_withholds_and_never_calls_bootstrap`.
//!      The ESTABLISHED-lineage path (the bulk of this module, described
//!      above) is completely unchanged: divergence there still always maps
//!      to `NeedsOperatorRebaseline` and is never auto-resolved.

use std::path::Path;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::tool::RustTool;

use super::tools::{
    auto_baseline_enabled, ensure_push_boundary, history_backfill, history_bootstrap_first_push, history_status,
    history_sync, remote_env_override, PushBoundary,
};

/// Environment variable holding the parking-lot root directory that contains
/// one internal-`main` checkout per repo (`<root>/<repo>`), the SAME variable
/// `git_public_history_*` resolve `source` from when a caller doesn't pass it
/// explicitly (see `resolve_source` in [`super::tools`]). Reused here ONLY to
/// discover which subdirectories exist — never written to.
const SOURCE_ROOT_ENV: &str = "TERMINUS_MIRROR_SOURCE_ROOT";

/// Per-repo call overrides. Every field mirrors an optional arg the
/// `git_public_history_*` tools already accept; `None` lets each tool fall
/// back to its own env-var default (`TERMINUS_MIRROR_SOURCE_ROOT`,
/// `TERMINUS_MIRROR_REMOTE[_<REPO>]`) exactly as an operator-driven call
/// would.
#[derive(Debug, Clone, Default)]
pub struct RunnerConfig {
    pub source: Option<String>,
    pub github_remote: Option<String>,
    pub provider: Option<String>,
    /// MIRROR-AUTO: whether a repo with NO established public lineage may be
    /// automatically baselined (full backfill + full-history PII gate, then
    /// — ONLY if gate-clean AND the public remote genuinely has no `main`
    /// branch yet — a genuine, never-force initial push). Defaults to
    /// `false` via `#[derive(Default)]` so a bare `RunnerConfig::default()`
    /// (as many existing tests construct) never silently auto-publishes.
    /// PRODUCTION callers (`run_once`, `GitPublicMirrorRun::execute`) resolve
    /// this from `TERMINUS_MIRROR_AUTO_BASELINE` via
    /// `super::tools::auto_baseline_enabled()` instead of relying on this
    /// struct default — that env var itself defaults to TRUE per the
    /// operator directive. The mismatch is deliberate: the struct's zero
    /// value stays a safe `false`, the resolved production default is `true`.
    pub auto_baseline: bool,
}

impl RunnerConfig {
    fn args(&self, repo: &str) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("repo".into(), json!(repo));
        if let Some(s) = &self.source {
            m.insert("source".into(), json!(s));
        }
        if let Some(r) = &self.github_remote {
            m.insert("github_remote".into(), json!(r));
        }
        if let Some(p) = &self.provider {
            m.insert("provider".into(), json!(p));
        }
        Value::Object(m)
    }
}

/// One repo's outcome from a [`run_once`] pass.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RunOutcome {
    /// The public remote was confirmed at head by the sync step (a
    /// remote-checked no-op) — nothing to push. NOT inferred from
    /// history-status's source-vs-local `commits_behind` alone.
    UpToDate,
    /// The full-history PII gate (via backfill, or the sync tool's own
    /// incremental gate over newly-appended commits) reported residual
    /// violations. Nothing was pushed.
    GateDirty { residual_count: usize, residuals: Vec<Value> },
    /// A clean, fast-forward sync was pushed to the public mirror.
    Pushed { from: Option<String>, to: String },
    /// No established lineage, or the fast-forward analysis found the public
    /// mirror diverged / un-bootstrapped / ahead — this requires the
    /// one-time operator-blessed GHIST-07 force re-baseline. NEVER
    /// auto-resolved by this module.
    NeedsOperatorRebaseline { reason: String },
    /// MIRROR-AUTO security gate: the resolved push target could NOT be
    /// verified as an existing public repo under the configured mirror org —
    /// because an operator override (`github_remote` / `TERMINUS_MIRROR_REMOTE`)
    /// or the explicit-`repo` call pointed at a remote that failed
    /// `github::repo_exists` (repo_exists=false), a foreign/unparseable URL,
    /// or the existence check itself errored (→ fail-closed). NOTHING was
    /// pushed. This is the enforcement point that makes "we only ever push to
    /// a verified, name/org-matched public repo we own" hold on EVERY path,
    /// including auto-baseline — an unverified override can never redirect an
    /// auto-push at the wrong remote.
    SkippedUnverifiedRemote { remote: Option<String>, reason: String },
    /// A tool call failed for a reason that isn't a known divergence signal
    /// (e.g. a transient git/IO error, missing credential). Surfaced, not
    /// panicked.
    Error { message: String },
}

/// The full report for one repo, returned by [`run_once`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MirrorRunReport {
    pub repo: String,
    #[serde(flatten)]
    pub outcome: RunOutcome,
}

impl MirrorRunReport {
    fn error(repo: &str, e: &ToolError) -> Self {
        Self { repo: repo.to_string(), outcome: RunOutcome::Error { message: e.to_string() } }
    }

    fn needs_rebaseline(repo: &str, reason: impl Into<String>) -> Self {
        Self { repo: repo.to_string(), outcome: RunOutcome::NeedsOperatorRebaseline { reason: reason.into() } }
    }

    fn gate_dirty(repo: &str, gate: Option<&Value>) -> Self {
        let residuals = gate
            .and_then(|g| g.get("violations"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let residual_count = gate
            .and_then(|g| g.get("residual_count"))
            .and_then(Value::as_u64)
            .unwrap_or(residuals.len() as u64) as usize;
        Self { repo: repo.to_string(), outcome: RunOutcome::GateDirty { residual_count, residuals } }
    }
}

/// The three `git_public_history_*` calls [`run_once`] orchestrates, as a
/// trait so tests can inject stubbed status/gate/sync results without
/// spinning up a real git repo, GitHub credential, or `TERMINUS_MIRROR_*`
/// environment. [`RealHistoryOps`] is the production implementation — the
/// SAME `execute()` the tool registry dispatches to (via [`super::tools`]'s
/// wrappers), so nothing about git, PII scanning, or transport is
/// reimplemented here or in the trait.
#[async_trait]
pub trait HistoryOps: Send + Sync {
    async fn status(&self, repo: &str, cfg: &RunnerConfig) -> Result<Value, ToolError>;
    /// Establish the going-forward push boundary BEFORE backfill advances the
    /// local work-dir HEAD (MRUN-01 ff-detection fix — see
    /// [`ensure_push_boundary`]).
    async fn ensure_boundary(&self, repo: &str, cfg: &RunnerConfig) -> Result<PushBoundary, ToolError>;
    async fn backfill(&self, repo: &str, cfg: &RunnerConfig) -> Result<Value, ToolError>;
    async fn sync(&self, repo: &str, cfg: &RunnerConfig) -> Result<Value, ToolError>;
    /// MIRROR-AUTO: publish a genuinely first-time, already gate-clean
    /// snapshot to an empty public remote. Only ever called from the
    /// no-lineage branch of [`run_once_with`], and only after that branch's
    /// own `backfill` call reported `gate.clean == true` — see
    /// [`super::tools::bootstrap_first_push`] for the full safety contract
    /// (never force-pushes; refuses `Conflict` if the remote unexpectedly
    /// already has a `main` branch).
    async fn bootstrap_first_push(&self, repo: &str, cfg: &RunnerConfig) -> Result<Value, ToolError>;
}

/// Production [`HistoryOps`]: calls the real `git_public_history_*` tools.
pub struct RealHistoryOps;

fn parse_json(text: String) -> Result<Value, ToolError> {
    serde_json::from_str(&text)
        .map_err(|e| ToolError::Execution(format!("mirror-runner: non-JSON tool response: {e}")))
}

#[async_trait]
impl HistoryOps for RealHistoryOps {
    async fn status(&self, repo: &str, cfg: &RunnerConfig) -> Result<Value, ToolError> {
        parse_json(history_status(cfg.args(repo)).await?)
    }
    async fn ensure_boundary(&self, repo: &str, cfg: &RunnerConfig) -> Result<PushBoundary, ToolError> {
        ensure_push_boundary(&cfg.args(repo))
    }
    async fn backfill(&self, repo: &str, cfg: &RunnerConfig) -> Result<Value, ToolError> {
        parse_json(history_backfill(cfg.args(repo)).await?)
    }
    async fn sync(&self, repo: &str, cfg: &RunnerConfig) -> Result<Value, ToolError> {
        parse_json(history_sync(cfg.args(repo)).await?)
    }
    async fn bootstrap_first_push(&self, repo: &str, cfg: &RunnerConfig) -> Result<Value, ToolError> {
        history_bootstrap_first_push(cfg.args(repo)).await
    }
}

/// Run one idempotent mirror pass for `repo`. Safe to call on any cadence
/// (systemd timer, ad hoc) — every branch either no-ops or performs a single
/// fast-forward-only, gate-verified push.
///
/// Steps (see the module doc for the full safety rationale):
/// 1. `git_public_history_status` — no established lineage →
///    [`RunOutcome::NeedsOperatorRebaseline`] (a first baseline is an operator
///    action, never automatic). Its `commits_behind` is used ONLY to decide
///    whether a backfill is needed; it is NOT trusted for the up-to-date
///    decision, because it compares source-vs-local and never inspects the
///    public remote.
/// 1b. `ensure_push_boundary` — pins the going-forward push boundary before
///    backfill and confirms remote lineage (divergence / un-bootstrapped →
///    [`RunOutcome::NeedsOperatorRebaseline`]).
/// 2. `git_public_history_backfill` (only when the local mirror is behind
///    source) — replays + gates every commit. Residual PII →
///    [`RunOutcome::GateDirty`], nothing pushed.
/// 3. `git_public_history_sync` — the REMOTE-aware step. Fast-forward-only push
///    of the gate-clean result; a remote-checked no-op → [`RunOutcome::UpToDate`]
///    (the ONLY path that yields it); a [`ToolError::Conflict`] (diverged /
///    un-bootstrapped / non-fast-forward) → [`RunOutcome::NeedsOperatorRebaseline`];
///    residual PII in the unpublished range → [`RunOutcome::GateDirty`]; a
///    fast-forward push (including self-healing a prior failed push) →
///    [`RunOutcome::Pushed`].
pub async fn run_once(repo: &str, cfg: &RunnerConfig) -> MirrorRunReport {
    run_once_with(repo, cfg, &RealHistoryOps).await
}

/// [`run_once`], with the three `git_public_history_*` calls routed through an
/// injected [`HistoryOps`] — the seam the unit tests below use to exercise
/// every branch (up-to-date, gate-dirty, clean+ff, clean+divergent, sync
/// error) without touching git or the filesystem.
pub async fn run_once_with(repo: &str, cfg: &RunnerConfig, ops: &dyn HistoryOps) -> MirrorRunReport {
    // 1. Status.
    let status = match ops.status(repo, cfg).await {
        Ok(v) => v,
        Err(e) => return MirrorRunReport::error(repo, &e),
    };
    let lineage_established = status.get("lineage_established").and_then(Value::as_bool).unwrap_or(false);
    if !lineage_established {
        if !cfg.auto_baseline {
            return MirrorRunReport::needs_rebaseline(
                repo,
                "no established full-history lineage — run git_public_history_backfill and have the \
                 operator bless + force re-baseline the public mirror first (GHIST-07); the runner \
                 only extends an already-bootstrapped baseline, it never creates one (auto-baseline \
                 is disabled for this run — set TERMINUS_MIRROR_AUTO_BASELINE to enable it)",
            );
        }
        // MIRROR-AUTO auto-baseline: a genuinely first-time repo. Run the
        // SAME full-history backfill + full PII gate an operator-driven
        // GHIST-07 bootstrap would require a human to eyeball first. The PII
        // gate is the unconditional hard block here exactly as everywhere
        // else in this module — a dirty gate returns GateDirty and NOTHING
        // is pushed, regardless of auto_baseline. Only a gate-clean result
        // proceeds to `bootstrap_first_push`, which itself refuses
        // (NeedsOperatorRebaseline) rather than force-publishing if the
        // remote unexpectedly already has content.
        let backfill = match ops.backfill(repo, cfg).await {
            Ok(v) => v,
            Err(e) => return MirrorRunReport::error(repo, &e),
        };
        let gate = backfill.get("gate");
        let gate_clean = gate.and_then(|g| g.get("clean")).and_then(Value::as_bool).unwrap_or(false);
        if !gate_clean {
            return MirrorRunReport::gate_dirty(repo, gate);
        }
        return match ops.bootstrap_first_push(repo, cfg).await {
            Ok(v) => {
                let to = v.get("work_head").and_then(Value::as_str).unwrap_or_default().to_string();
                MirrorRunReport { repo: repo.to_string(), outcome: RunOutcome::Pushed { from: None, to } }
            }
            Err(ToolError::Conflict(reason)) => MirrorRunReport::needs_rebaseline(repo, reason),
            Err(e) => MirrorRunReport::error(repo, &e),
        };
    }
    // `commits_behind` from status compares SOURCE vs the LOCAL history work-dir
    // ONLY — it does NOT inspect the public remote or the `pushed-head` boundary.
    // So `commits_behind == 0` means "the local mirror has replayed every source
    // commit", NOT "the public mirror is current": a prior tick may have replayed
    // but FAILED to push, or the remote may have been rewound / diverged. Returning
    // up_to_date here (as an earlier version did) would let a behind/failed-push
    // mirror silently stay behind forever, defeating the runner's entire purpose.
    // Instead we ALWAYS proceed to the remote-aware sync below; `up_to_date` is only
    // ever returned when SYNC confirms the remote is at head (MRUN-01 self-heal).
    let source_behind = status.get("commits_behind").and_then(Value::as_u64);

    // 1b. Establish the push boundary BEFORE any backfill (MRUN-01 ff-detection fix).
    // Backfill (step 2) advances the local history work-dir HEAD; if the
    // going-forward `pushed-head` marker isn't already set, git_public_history_sync
    // would try to initialise it from the POST-backfill HEAD and spuriously see a
    // non-fast-forward. Pinning the boundary to the pre-backfill baseline here keeps
    // sync's ff-detection correct so a genuinely fast-forwardable repo actually gets
    // pushed. This step also confirms remote lineage: genuine divergence /
    // un-bootstrapped remote → needs_operator_rebaseline, reported WITHOUT running
    // backfill and WITHOUT ever forcing.
    match ops.ensure_boundary(repo, cfg).await {
        Ok(PushBoundary::Established) => {}
        Ok(PushBoundary::NeedsOperator(reason)) => {
            return MirrorRunReport::needs_rebaseline(repo, reason);
        }
        Err(e) => return MirrorRunReport::error(repo, &e),
    }

    // 2. Backfill (replay new source commits + full-history gate) — ONLY when the
    // local mirror is actually behind source. When `source_behind == 0` there is
    // nothing new to replay, so the expensive full-history gate is skipped and we go
    // straight to sync; sync's own incremental gate still scans (and can withhold)
    // any commits that were replayed by a prior tick but never published. NEVER pushes.
    if source_behind != Some(0) {
        let backfill = match ops.backfill(repo, cfg).await {
            Ok(v) => v,
            Err(e) => return MirrorRunReport::error(repo, &e),
        };
        let gate = backfill.get("gate");
        let gate_clean = gate.and_then(|g| g.get("clean")).and_then(Value::as_bool).unwrap_or(false);
        if !gate_clean {
            return MirrorRunReport::gate_dirty(repo, gate);
        }
    }

    // 3. Fast-forward-only sync/push — the REMOTE-aware step. It cleanly no-ops
    // (up_to_date) ONLY when the remote is confirmed at head, ff-pushes when the
    // remote is behind (self-heal after a prior failed push), withholds on residual
    // PII in the unpublished range, and Conflicts (→ needs_operator) on divergence.
    let sync = match ops.sync(repo, cfg).await {
        Ok(v) => v,
        Err(ToolError::Conflict(reason)) => return MirrorRunReport::needs_rebaseline(repo, reason),
        Err(e) => return MirrorRunReport::error(repo, &e),
    };
    if sync.get("withheld").and_then(Value::as_bool).unwrap_or(false) {
        return MirrorRunReport::gate_dirty(repo, sync.get("gate"));
    }
    if sync.get("pushed").and_then(Value::as_bool).unwrap_or(false) {
        let to = sync.get("work_head").and_then(Value::as_str).unwrap_or_default().to_string();
        let from = sync.get("old_head").and_then(Value::as_str).map(str::to_string);
        return MirrorRunReport { repo: repo.to_string(), outcome: RunOutcome::Pushed { from, to } };
    }
    if sync.get("up_to_date").and_then(Value::as_bool).unwrap_or(false) {
        return MirrorRunReport { repo: repo.to_string(), outcome: RunOutcome::UpToDate };
    }
    // An unrecognised (but successful) response shape — surfaced, not guessed at.
    MirrorRunReport::error(
        repo,
        &ToolError::Execution(format!("mirror-runner: unrecognised sync response shape: {sync}")),
    )
}

// ── MIRROR-AUTO opt-out repo discovery (for the no-`repo`-arg "all repos" mode) ─
//
// Pre-MIRROR-AUTO this was `discover_mirror_ready_repos`, requiring an
// explicit `mirror_ready: true` opt-in in each repo's `.moosenet-pipeline.yaml`
// (fail-CLOSED on absence). MIRROR-AUTO flips the default to OPT-OUT: every
// repo under TERMINUS_MIRROR_SOURCE_ROOT is a mirror CANDIDATE unless
// blacklisted or explicitly `mirror_ready: false`, and a candidate only
// actually becomes a mirror TARGET once `discovery::discover_public_remote`
// confirms a real `moosenet-io/<repo>` (or name-mapped/org-configured
// equivalent) already exists on GitHub — that existence check IS the
// opt-out: an operator who never created/publicized the public repo simply
// never sees it mirrored, no YAML edit required. See `discovery`'s module
// doc for the fail-closed-on-error posture of that check.

/// Explicit opt-out: `.moosenet-pipeline.yaml` sets `mirror_ready: false`.
/// Belt-and-suspenders on top of the discovery-driven opt-out above — kept so
/// a repo can be excluded even if it (surprisingly) already has a same-named
/// public GitHub repo the operator does NOT want auto-mirrored. Missing
/// file, unparsable YAML, or an ABSENT `mirror_ready` key are all "not
/// explicitly opted out" (the repo stays a candidate) — this is the INVERSE
/// fail posture of the old opt-in check: MIRROR-AUTO fails OPEN (stays a
/// candidate) on absence and fails CLOSED (excluded) only on an explicit
/// `false`.
fn repo_explicitly_opted_out(checkout: &Path) -> bool {
    let path = checkout.join(".moosenet-pipeline.yaml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&text) else {
        return false;
    };
    matches!(doc.get("mirror_ready").and_then(serde_yaml::Value::as_bool), Some(false))
}

/// List every immediate subdirectory of `TERMINUS_MIRROR_SOURCE_ROOT` that is
/// a mirror CANDIDATE: not blacklisted (`TERMINUS_MIRROR_BLACKLIST`) and not
/// explicitly opted out (`mirror_ready: false`). Does NOT check public-repo
/// existence — that's [`resolve_and_verify_remote`]'s job, kept separate so
/// this blacklist/opt-out logic can be tested without any network seam.
/// READ-ONLY scan; sorted + deduplicated for stable, reproducible runs.
fn list_mirror_candidates() -> Result<Vec<String>, ToolError> {
    let root = std::env::var(SOURCE_ROOT_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ToolError::NotConfigured(format!(
                "no 'repo' was given and {SOURCE_ROOT_ENV} is not set — pass 'repo' explicitly or \
                 configure {SOURCE_ROOT_ENV} so every mirror-candidate repo under it can be discovered"
            ))
        })?;
    let entries = std::fs::read_dir(&root)
        .map_err(|e| ToolError::Execution(format!("read {SOURCE_ROOT_ENV} ({root}): {e}")))?;
    let blacklist = super::discovery::blacklist();
    let mut repos: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| ToolError::Execution(format!("read_dir entry: {e}")))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if blacklist.contains(name) {
            continue;
        }
        if repo_explicitly_opted_out(&path) {
            continue;
        }
        repos.push(name.to_string());
    }
    repos.sort();
    repos.dedup();
    Ok(repos)
}

// ── Verified per-repo remote resolution (closes the two verification-bypass gaps) ─
//
// codex security finding: the public-repo existence VERIFICATION must hold on
// EVERY push path, not only the discovered-remote path. Two bypasses existed:
//   1. OVERRIDE bypass — in bulk mode an override (`github_remote` /
//      `TERMINUS_MIRROR_REMOTE[_<REPO>]`) replaced the discovered (verified)
//      remote without re-verifying the override.
//   2. EXPLICIT-REPO bypass — an explicit `repo` arg skipped discovery
//      entirely, so it could auto-baseline to a configured remote that was
//      never proven to exist.
// [`resolve_and_verify_remote`] is the single funnel every repo now passes
// through: whatever remote we would push to (override OR discovered) is
// verified via `github::repo_exists` (org-matched + exists) BEFORE the repo
// ever reaches `run_once`/bootstrap. An unverifiable target yields
// [`RemoteResolution::Rejected`] and NO push.

/// The verified push-target decision for one repo, produced BEFORE any
/// push/bootstrap can run.
#[derive(Debug, PartialEq)]
enum RemoteResolution {
    /// A verified public target under the configured org — safe to push here.
    Verified(String),
    /// An override (or explicitly-targeted) remote was configured but FAILED
    /// verification (repo_exists=false, foreign/unparseable URL, or the check
    /// errored → fail-closed). Do NOT push; surface as `SkippedUnverifiedRemote`.
    Rejected { remote: String, reason: String },
    /// No override was set and name-based discovery found no existing public
    /// target — a plain opt-out. In bulk mode this repo is silently skipped
    /// (exactly as before); for an explicit single-repo call it is surfaced
    /// as `SkippedUnverifiedRemote` so the caller learns why nothing happened.
    NoTarget,
}

/// Resolve AND verify the push remote for `repo`. Precedence:
///   1. an override — call-level `explicit_remote` (the tool's `github_remote`
///      arg), else `TERMINUS_MIRROR_REMOTE[_<REPO>]` env — which is honored
///      ONLY if it passes [`super::discovery::verify_public_remote`]
///      (org-matched + `repo_exists`); otherwise `Rejected`.
///   2. no override → name-based discovery
///      ([`super::discovery::discover_public_remote_with`]), which itself only
///      yields a remote when the public repo exists, so a discovered remote is
///      verified by construction; no match → `NoTarget`.
/// Either way, the ONLY variant that leads to a push is `Verified` — every
/// push path is now behind existence verification.
async fn resolve_and_verify_remote(
    verifier: &dyn super::discovery::PublicRepoExists,
    repo: &str,
    explicit_remote: Option<&str>,
) -> RemoteResolution {
    let override_remote = explicit_remote.map(str::to_string).or_else(|| remote_env_override(repo));
    match override_remote {
        Some(remote) => match super::discovery::verify_public_remote(verifier, &remote).await {
            Ok(()) => RemoteResolution::Verified(remote),
            Err(reason) => RemoteResolution::Rejected { remote, reason },
        },
        None => match super::discovery::discover_public_remote_with(verifier, repo).await {
            Some(remote) => RemoteResolution::Verified(remote),
            None => RemoteResolution::NoTarget,
        },
    }
}

// ── git_public_mirror_run (core tool) ───────────────────────────────────────

/// `git_public_mirror_run` — the MRUN-01 tool wrapping [`run_once`]. With an
/// explicit `repo`, runs one pass for that repo; without one, iterates the
/// MIRROR-AUTO opt-out candidate list ([`list_mirror_candidates`]). On BOTH
/// paths, every repo's push target is resolved AND verified via
/// [`resolve_and_verify_remote`] before any push — an override or explicit
/// target that fails `github::repo_exists` is reported as
/// `SkippedUnverifiedRemote` and never pushed. Returns a per-repo report
/// array — this is the call `deploy/terminus-mirror-runner.service` makes on
/// a timer.
pub(crate) struct GitPublicMirrorRun;

#[async_trait]
impl RustTool for GitPublicMirrorRun {
    fn name(&self) -> &str {
        "git_public_mirror_run"
    }

    fn description(&self) -> &str {
        "MRUN-01/MIRROR-AUTO. Run one idempotent git-public mirror pass: read \
         git_public_history_status, and if behind, run git_public_history_backfill \
         (replay + full-history PII gate, never pushes) then, only when gate-clean, \
         git_public_history_sync (fast-forward-only push of an already \
         operator-blessed baseline) — OR, for a genuinely first-time repo with \
         TERMINUS_MIRROR_AUTO_BASELINE enabled (default true), an automatic gate-clean \
         first publish to an empty remote. NEVER force-pushes: a diverged / \
         un-bootstrapped / non-fast-forward mirror, or a remote that unexpectedly \
         already has content, is reported as needing the one-time operator-blessed \
         re-baseline rather than acted on. Residual PII always withholds the push, on \
         every path, unconditionally. With no 'repo', runs MIRROR-AUTO opt-out \
         discovery over TERMINUS_MIRROR_SOURCE_ROOT (every repo there is a candidate \
         unless blacklisted via TERMINUS_MIRROR_BLACKLIST or explicitly \
         mirror_ready:false, and only repos with a confirmed public GitHub target — \
         see TERMINUS_MIRROR_GITHUB_ORG / TERMINUS_MIRROR_NAME_MAP — are actually run) \
         and returns one report per discovered repo. Intended to be driven by \
         deploy/terminus-mirror-runner.timer."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":          { "type": "string", "description": "Logical repo name; omit to run MIRROR-AUTO discovery over every candidate under TERMINUS_MIRROR_SOURCE_ROOT" },
                "source":        { "type": "string", "description": "internal-main checkout override (else TERMINUS_MIRROR_SOURCE_ROOT/<repo>)" },
                "github_remote": { "type": "string", "description": "Target mirror remote override for ALL repos in this call (else per-repo TERMINUS_MIRROR_REMOTE[_<REPO>], else the MIRROR-AUTO discovered remote). An override is honored ONLY if it points to an existing repo under the configured org (github::repo_exists) — otherwise the repo is skipped, never pushed." },
                "provider":      { "type": "string", "description": "Mirror-push target provider (default 'github')" }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let source = args.get("source").and_then(Value::as_str).map(str::to_string);
        let explicit_remote =
            args.get("github_remote").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        let provider = args.get("provider").and_then(Value::as_str).map(str::to_string);
        let auto_baseline = auto_baseline_enabled();

        // Explicit single-repo call vs. bulk opt-out candidate list. In BOTH
        // cases each repo's target is resolved+verified below before any push.
        let explicit_repo = args.get("repo").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
        let (repos, is_explicit): (Vec<String>, bool) = match explicit_repo {
            Some(r) => (vec![r.to_string()], true),
            None => (list_mirror_candidates()?, false),
        };

        let verifier = super::discovery::RealPublicRepoExists;
        let mut reports = Vec::with_capacity(repos.len());
        for repo in &repos {
            // SECURITY GATE (codex fix): resolve the FINAL push remote and
            // VERIFY it on every path. Only `Verified` reaches run_once — an
            // override/explicit target that can't be proven to exist under the
            // configured org never gets an auto-baseline or any push.
            match resolve_and_verify_remote(&verifier, repo, explicit_remote.as_deref()).await {
                RemoteResolution::Verified(remote) => {
                    let cfg = RunnerConfig {
                        source: source.clone(),
                        github_remote: Some(remote),
                        provider: provider.clone(),
                        auto_baseline,
                    };
                    reports.push(run_once(repo, &cfg).await);
                }
                RemoteResolution::Rejected { remote, reason } => {
                    tracing::warn!(
                        target: "mirror_audit",
                        event = "skipped_unverified_remote",
                        repo = %repo,
                        remote = %remote,
                        reason = %reason,
                        "MIRROR-AUTO: refused to push — configured override/target remote did not verify"
                    );
                    reports.push(MirrorRunReport {
                        repo: repo.clone(),
                        outcome: RunOutcome::SkippedUnverifiedRemote { remote: Some(remote), reason },
                    });
                }
                RemoteResolution::NoTarget => {
                    if is_explicit {
                        // The caller asked for THIS repo by name; tell them why
                        // nothing happened rather than silently dropping it.
                        reports.push(MirrorRunReport {
                            repo: repo.clone(),
                            outcome: RunOutcome::SkippedUnverifiedRemote {
                                remote: None,
                                reason: "no verified public mirror target exists for this repo (no override set and \
                                         github::repo_exists found no matching public repo under the configured org) \
                                         — refusing to auto-baseline/push to an unverified target"
                                    .into(),
                            },
                        });
                    }
                    // Bulk mode: a candidate with no public target is a plain
                    // opt-out — silently skipped, exactly as before.
                }
            }
        }

        serde_json::to_string(&json!({ "repos_run": reports.len(), "reports": reports }))
            .map_err(|e| ToolError::Execution(format!("serialize reports: {e}")))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ToolRegistry;
    use serial_test::serial;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A stubbed [`HistoryOps`] whose three calls return pre-scripted results
    /// (or errors) and count how many times each was invoked — the negative
    /// assertions below (e.g. "sync is never called after a gate-dirty
    /// backfill") depend on those counts.
    #[derive(Default)]
    struct StubOps {
        status: Option<Result<Value, ToolError>>,
        /// `None` → the boundary is `Established` (the common ff-able case), so
        /// existing tests that don't care about the boundary keep passing. Set to
        /// `NeedsOperator`/`Err` to exercise the pre-backfill short-circuit.
        boundary: Option<PushBoundary>,
        boundary_err: Option<ToolError>,
        backfill: Option<Result<Value, ToolError>>,
        sync: Option<Result<Value, ToolError>>,
        /// MIRROR-AUTO: the stubbed `bootstrap_first_push` result. `None`
        /// stub with the method actually invoked panics loudly (same
        /// "stub not set" discipline as the other four), so a test that
        /// doesn't expect this call to happen must assert `bootstrap_calls
        /// == 0` rather than relying on a default.
        bootstrap: Option<Result<Value, ToolError>>,
        status_calls: AtomicUsize,
        boundary_calls: AtomicUsize,
        backfill_calls: AtomicUsize,
        sync_calls: AtomicUsize,
        bootstrap_calls: AtomicUsize,
    }

    /// Clone a stubbed `Result`, preserving the `ToolError` VARIANT (not just
    /// its message) — `run_once_with` branches on `Conflict` specifically
    /// (never-force divergence signal) vs. every other variant (a plain
    /// error), so collapsing everything to one variant here would silently
    /// break the very distinction these tests exist to check.
    fn clone_result(r: &Result<Value, ToolError>) -> Result<Value, ToolError> {
        match r {
            Ok(v) => Ok(v.clone()),
            Err(ToolError::Conflict(m)) => Err(ToolError::Conflict(m.clone())),
            Err(ToolError::NotConfigured(m)) => Err(ToolError::NotConfigured(m.clone())),
            Err(ToolError::InvalidArgument(m)) => Err(ToolError::InvalidArgument(m.clone())),
            Err(ToolError::NotFound(m)) => Err(ToolError::NotFound(m.clone())),
            Err(ToolError::Http(m)) => Err(ToolError::Http(m.clone())),
            Err(ToolError::Database(m)) => Err(ToolError::Database(m.clone())),
            Err(ToolError::Execution(m)) => Err(ToolError::Execution(m.clone())),
        }
    }

    #[async_trait]
    impl HistoryOps for StubOps {
        async fn status(&self, _repo: &str, _cfg: &RunnerConfig) -> Result<Value, ToolError> {
            self.status_calls.fetch_add(1, Ordering::SeqCst);
            clone_result(self.status.as_ref().expect("status stub not set"))
        }
        async fn ensure_boundary(&self, _repo: &str, _cfg: &RunnerConfig) -> Result<PushBoundary, ToolError> {
            self.boundary_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(e) = &self.boundary_err {
                return Err(ToolError::Execution(e.to_string()));
            }
            Ok(match &self.boundary {
                Some(PushBoundary::NeedsOperator(m)) => PushBoundary::NeedsOperator(m.clone()),
                _ => PushBoundary::Established,
            })
        }
        async fn backfill(&self, _repo: &str, _cfg: &RunnerConfig) -> Result<Value, ToolError> {
            self.backfill_calls.fetch_add(1, Ordering::SeqCst);
            clone_result(self.backfill.as_ref().expect("backfill stub not set"))
        }
        async fn sync(&self, _repo: &str, _cfg: &RunnerConfig) -> Result<Value, ToolError> {
            self.sync_calls.fetch_add(1, Ordering::SeqCst);
            clone_result(self.sync.as_ref().expect("sync stub not set"))
        }
        async fn bootstrap_first_push(&self, _repo: &str, _cfg: &RunnerConfig) -> Result<Value, ToolError> {
            self.bootstrap_calls.fetch_add(1, Ordering::SeqCst);
            clone_result(self.bootstrap.as_ref().expect("bootstrap stub not set"))
        }
    }

    fn status_json(lineage: bool, behind: Option<u64>) -> Value {
        json!({
            "repo": "demo",
            "lineage_established": lineage,
            "source_head": "deadbeef",
            "source_commits": 10,
            "work_commits": 10,
            "last_mirrored_sha": if lineage { Some("deadbeef") } else { None },
            "commits_behind": behind,
        })
    }

    /// (c) Remote genuinely at head. `source_behind == 0` skips backfill, but the
    /// runner STILL calls sync to confirm the REMOTE is current — only sync's
    /// `up_to_date` (a remote-checked no-op) yields `UpToDate`. status alone is
    /// never trusted for the up-to-date decision.
    #[tokio::test]
    async fn remote_at_head_reports_up_to_date_via_sync_not_status() {
        let ops = StubOps {
            status: Some(Ok(status_json(true, Some(0)))),
            sync: Some(Ok(json!({
                "repo": "demo",
                "pushed": false,
                "up_to_date": true,
                "new_commits": 0,
                "work_head": "head000",
            }))),
            ..Default::default()
        };
        let report = run_once_with("demo", &RunnerConfig::default(), &ops).await;
        assert_eq!(report.outcome, RunOutcome::UpToDate);
        assert_eq!(ops.status_calls.load(Ordering::SeqCst), 1);
        // Nothing new to replay → backfill skipped, but the remote WAS checked (sync).
        assert_eq!(ops.backfill_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ops.boundary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.sync_calls.load(Ordering::SeqCst), 1);
    }

    /// (a) Local mirror has replayed every source commit (`source_behind == 0`) but
    /// the REMOTE is behind (a prior tick pushed-head lag / rewound remote). status
    /// would say "0 behind", yet the runner must NOT report up_to_date — it reaches
    /// sync, which ff-pushes the unpublished commits. Backfill is skipped (nothing
    /// new to replay); the push still happens.
    #[tokio::test]
    async fn local_current_but_remote_behind_pushes_not_up_to_date() {
        let ops = StubOps {
            status: Some(Ok(status_json(true, Some(0)))),
            sync: Some(Ok(json!({
                "repo": "demo",
                "pushed": true,
                "new_commits": 1,
                "old_head": "remote0",
                "work_head": "local01",
                "branch": "main",
            }))),
            ..Default::default()
        };
        let report = run_once_with("demo", &RunnerConfig::default(), &ops).await;
        match report.outcome {
            RunOutcome::Pushed { from, to } => {
                assert_eq!(from.as_deref(), Some("remote0"));
                assert_eq!(to, "local01");
            }
            other => panic!("a behind remote must push even when source_behind==0, got {other:?}"),
        }
        assert_eq!(ops.backfill_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ops.sync_calls.load(Ordering::SeqCst), 1);
    }

    /// (b) Idempotent self-heal: a PRIOR tick advanced the local mirror (backfill +
    /// replay) but its push FAILED, so local == source (`source_behind == 0`) while
    /// the remote sits behind. Every subsequent tick must retry the push (not report
    /// up_to_date and exit success forever). Same stub shape as (a) but framed as the
    /// repeat tick: sync pushes the still-unpublished range.
    #[tokio::test]
    async fn prior_failed_push_is_retried_on_next_tick() {
        let ops = StubOps {
            status: Some(Ok(status_json(true, Some(0)))),
            sync: Some(Ok(json!({
                "repo": "demo",
                "pushed": true,
                "new_commits": 3,
                "old_head": "pushed00",
                "work_head": "worktip9",
                "branch": "main",
            }))),
            ..Default::default()
        };
        let report = run_once_with("demo", &RunnerConfig::default(), &ops).await;
        assert!(matches!(report.outcome, RunOutcome::Pushed { .. }), "self-heal must push: {report:?}");
        assert_eq!(ops.backfill_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ops.sync_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn no_lineage_needs_operator_rebaseline_never_calls_backfill_or_sync() {
        // auto_baseline defaults to `false` on a bare RunnerConfig::default()
        // (see its doc comment) — so this preserves the PRE-MIRROR-AUTO
        // behavior exactly: no backfill, no sync, no bootstrap attempt at all.
        let ops = StubOps { status: Some(Ok(status_json(false, None))), ..Default::default() };
        let report = run_once_with("demo", &RunnerConfig::default(), &ops).await;
        assert!(matches!(report.outcome, RunOutcome::NeedsOperatorRebaseline { .. }));
        assert_eq!(ops.backfill_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ops.sync_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ops.bootstrap_calls.load(Ordering::SeqCst), 0);
    }

    // ── MIRROR-AUTO: auto-baseline (no-lineage, cfg.auto_baseline == true) ────

    fn auto_baseline_cfg() -> RunnerConfig {
        RunnerConfig { auto_baseline: true, ..Default::default() }
    }

    /// AC: "First-time no-lineage + PII-clean -> auto-baseline + push (no
    /// operator gate), behind TERMINUS_MIRROR_AUTO_BASELINE." With the flag
    /// on (via `cfg.auto_baseline`) and backfill's full-history gate clean,
    /// the runner must call bootstrap_first_push and report Pushed with
    /// `from: None` (a genuine first publish, nothing preceded it) — and
    /// must NEVER call the established-lineage `sync`/`ensure_boundary`
    /// path for this branch.
    #[tokio::test]
    async fn auto_baseline_clean_gate_pushes_via_bootstrap_not_sync() {
        let ops = StubOps {
            status: Some(Ok(status_json(false, None))),
            backfill: Some(Ok(json!({
                "repo": "demo",
                "mode": "full-backfill",
                "gate": {"clean": true, "commits_scanned": 12, "unique_trees": 12, "residual_count": 0, "violations": []},
                "blessable": true,
            }))),
            bootstrap: Some(Ok(json!({
                "repo": "demo",
                "pushed": true,
                "bootstrap": true,
                "old_head": Value::Null,
                "work_head": "firstpush01",
                "branch": "main",
            }))),
            ..Default::default()
        };
        let report = run_once_with("demo", &auto_baseline_cfg(), &ops).await;
        match report.outcome {
            RunOutcome::Pushed { from, to } => {
                assert_eq!(from, None, "a genuine first publish has no prior 'from' head");
                assert_eq!(to, "firstpush01");
            }
            other => panic!("expected Pushed (auto-baseline), got {other:?}"),
        }
        assert_eq!(ops.backfill_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.bootstrap_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.sync_calls.load(Ordering::SeqCst), 0, "auto-baseline never goes through the established-lineage sync path");
        assert_eq!(ops.boundary_calls.load(Ordering::SeqCst), 0, "auto-baseline never calls ensure_boundary (that's for already-established lineage)");
    }

    /// AC: "Residual PII -> withheld, never pushed, even on the auto-baseline
    /// path." A dirty full-history gate from backfill must short-circuit to
    /// GateDirty and NEVER reach bootstrap_first_push — the PII hard block
    /// is unconditional, auto_baseline never weakens it.
    #[tokio::test]
    async fn auto_baseline_gate_dirty_withholds_and_never_calls_bootstrap() {
        let ops = StubOps {
            status: Some(Ok(status_json(false, None))),
            backfill: Some(Ok(json!({
                "repo": "demo",
                "mode": "full-backfill",
                "gate": {
                    "clean": false, "commits_scanned": 12, "unique_trees": 12, "residual_count": 1,
                    "violations": [{"commit": "aaa", "file": "x.txt", "line": 1, "pattern_kind": "ipv4", "context": "***"}],
                },
                "blessable": false,
            }))),
            ..Default::default()
        };
        let report = run_once_with("demo", &auto_baseline_cfg(), &ops).await;
        match report.outcome {
            RunOutcome::GateDirty { residual_count, .. } => assert_eq!(residual_count, 1),
            other => panic!("expected GateDirty, got {other:?}"),
        }
        assert_eq!(ops.backfill_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            ops.bootstrap_calls.load(Ordering::SeqCst),
            0,
            "residual PII must withhold the push unconditionally, even with auto_baseline on"
        );
    }

    /// AC: "Diverged -> still NeedsOperatorRebaseline, never auto-forced."
    /// Here the divergence signal comes from `bootstrap_first_push` itself
    /// refusing (a `Conflict`, e.g. because the "empty" remote unexpectedly
    /// already has a `main` branch) — the runner must map that to
    /// NeedsOperatorRebaseline, exactly like every other Conflict signal in
    /// this module, and never retry or force.
    #[tokio::test]
    async fn auto_baseline_bootstrap_conflict_needs_operator_never_forces() {
        let ops = StubOps {
            status: Some(Ok(status_json(false, None))),
            backfill: Some(Ok(json!({
                "repo": "demo",
                "gate": {"clean": true, "commits_scanned": 1, "unique_trees": 1, "residual_count": 0, "violations": []},
            }))),
            bootstrap: Some(Err(ToolError::Conflict(
                "public mirror 'main' for 'demo' already exists at deadbeef — refusing auto-baseline".into(),
            ))),
            ..Default::default()
        };
        let report = run_once_with("demo", &auto_baseline_cfg(), &ops).await;
        match report.outcome {
            RunOutcome::NeedsOperatorRebaseline { reason } => assert!(reason.contains("already exists")),
            other => panic!("expected NeedsOperatorRebaseline, got {other:?}"),
        }
        assert_eq!(ops.bootstrap_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.sync_calls.load(Ordering::SeqCst), 0);
    }

    /// A non-Conflict bootstrap error (transient IO/git failure) surfaces as
    /// Error, not panic, and not a mis-classified NeedsOperatorRebaseline.
    #[tokio::test]
    async fn auto_baseline_bootstrap_other_error_surfaces_as_error() {
        let ops = StubOps {
            status: Some(Ok(status_json(false, None))),
            backfill: Some(Ok(json!({
                "repo": "demo",
                "gate": {"clean": true, "commits_scanned": 1, "unique_trees": 1, "residual_count": 0, "violations": []},
            }))),
            bootstrap: Some(Err(ToolError::Execution("git push failed: connection reset".into()))),
            ..Default::default()
        };
        let report = run_once_with("demo", &auto_baseline_cfg(), &ops).await;
        match report.outcome {
            RunOutcome::Error { message } => assert!(message.contains("connection reset")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn gate_dirty_backfill_reports_residuals_and_never_calls_sync() {
        let ops = StubOps {
            status: Some(Ok(status_json(true, Some(3)))),
            backfill: Some(Ok(json!({
                "repo": "demo",
                "mode": "incremental",
                "gate": {
                    "clean": false,
                    "commits_scanned": 3,
                    "unique_trees": 3,
                    "residual_count": 2,
                    "violations": [
                        {"commit": "aaa", "file": "x.txt", "line": 1, "pattern_kind": "ipv4", "context": "***"},
                        {"commit": "bbb", "file": "y.txt", "line": 2, "pattern_kind": "ipv4", "context": "***"},
                    ],
                },
                "blessable": false,
            }))),
            ..Default::default()
        };
        let report = run_once_with("demo", &RunnerConfig::default(), &ops).await;
        match report.outcome {
            RunOutcome::GateDirty { residual_count, residuals } => {
                assert_eq!(residual_count, 2);
                assert_eq!(residuals.len(), 2);
            }
            other => panic!("expected GateDirty, got {other:?}"),
        }
        assert_eq!(ops.sync_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn clean_and_fast_forward_pushes_exactly_once() {
        let ops = StubOps {
            status: Some(Ok(status_json(true, Some(2)))),
            backfill: Some(Ok(json!({
                "repo": "demo",
                "mode": "incremental",
                "gate": {"clean": true, "commits_scanned": 2, "unique_trees": 2, "residual_count": 0, "violations": []},
                "blessable": true,
            }))),
            sync: Some(Ok(json!({
                "repo": "demo",
                "pushed": true,
                "new_commits": 2,
                "old_head": "aaa111",
                "work_head": "bbb222",
                "pushed_head": "bbb222",
                "branch": "main",
            }))),
            ..Default::default()
        };
        let report = run_once_with("demo", &RunnerConfig::default(), &ops).await;
        match report.outcome {
            RunOutcome::Pushed { from, to } => {
                assert_eq!(from.as_deref(), Some("aaa111"));
                assert_eq!(to, "bbb222");
            }
            other => panic!("expected Pushed, got {other:?}"),
        }
        assert_eq!(ops.backfill_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.sync_calls.load(Ordering::SeqCst), 1);
    }

    /// MRUN-01 ff-detection fix (regression for the under-push bug): an
    /// established-baseline repo that is BEHIND but genuinely fast-forwardable
    /// (boundary `Established`, no divergence) must end up `pushed`, NOT
    /// `needs_operator_rebaseline`. Before the fix, the runner's backfill
    /// advanced local HEAD and sync's first-run boundary init spuriously saw a
    /// non-ff; establishing the boundary pre-backfill keeps it ff-able.
    #[tokio::test]
    async fn established_baseline_behind_but_ff_able_pushes_not_needs_operator() {
        let ops = StubOps {
            status: Some(Ok(status_json(true, Some(2)))),
            // The pre-backfill boundary check succeeds (remote at the blessed baseline).
            boundary: Some(PushBoundary::Established),
            backfill: Some(Ok(json!({
                "repo": "demo",
                "mode": "incremental",
                "gate": {"clean": true, "commits_scanned": 2, "unique_trees": 2, "residual_count": 0, "violations": []},
                "blessable": true,
            }))),
            sync: Some(Ok(json!({
                "repo": "demo",
                "pushed": true,
                "new_commits": 2,
                "old_head": "base000",
                "work_head": "tip999",
                "branch": "main",
            }))),
            ..Default::default()
        };
        let report = run_once_with("demo", &RunnerConfig::default(), &ops).await;
        match report.outcome {
            RunOutcome::Pushed { to, .. } => assert_eq!(to, "tip999"),
            other => panic!("a fast-forwardable behind repo must push, got {other:?}"),
        }
        // The boundary was established (once) BEFORE backfill ran.
        assert_eq!(ops.boundary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.backfill_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.sync_calls.load(Ordering::SeqCst), 1);
    }

    /// A boundary check that reports genuine divergence short-circuits to
    /// `needs_operator_rebaseline` BEFORE backfill or sync are ever called —
    /// the runner never advances the work-dir toward a push it can't ff.
    #[tokio::test]
    async fn diverged_boundary_needs_operator_before_backfill_or_sync() {
        let ops = StubOps {
            status: Some(Ok(status_json(true, Some(1)))),
            boundary: Some(PushBoundary::NeedsOperator(
                "public mirror 'main' is at deadbeef, which is not an ancestor of the local \
                 blessed baseline cafe0000 — the mirror has diverged"
                    .into(),
            )),
            ..Default::default()
        };
        let report = run_once_with("demo", &RunnerConfig::default(), &ops).await;
        match report.outcome {
            RunOutcome::NeedsOperatorRebaseline { reason } => assert!(reason.contains("diverged")),
            other => panic!("expected NeedsOperatorRebaseline, got {other:?}"),
        }
        assert_eq!(ops.boundary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ops.backfill_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ops.sync_calls.load(Ordering::SeqCst), 0);
    }

    /// Clean + divergent: `git_public_history_sync` itself refuses a
    /// non-fast-forward move with a `Conflict`. The runner must map that to
    /// `NeedsOperatorRebaseline` and must NEVER retry with any force path —
    /// there is no second call, no alternate sync, nothing but a report.
    #[tokio::test]
    async fn clean_and_divergent_needs_operator_rebaseline_and_never_forces() {
        let ops = StubOps {
            status: Some(Ok(status_json(true, Some(1)))),
            backfill: Some(Ok(json!({
                "repo": "demo",
                "mode": "incremental",
                "gate": {"clean": true, "commits_scanned": 1, "unique_trees": 1, "residual_count": 0, "violations": []},
                "blessable": true,
            }))),
            sync: Some(Err(ToolError::Conflict(
                "non-fast-forward: mirror 'main' is at deadbeef, which is not an ancestor of the \
                 new tip cafef00d"
                    .into(),
            ))),
            ..Default::default()
        };
        let report = run_once_with("demo", &RunnerConfig::default(), &ops).await;
        match report.outcome {
            RunOutcome::NeedsOperatorRebaseline { reason } => {
                assert!(reason.contains("non-fast-forward"));
            }
            other => panic!("expected NeedsOperatorRebaseline, got {other:?}"),
        }
        assert_eq!(ops.sync_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn sync_withheld_reports_gate_dirty_not_pushed() {
        let ops = StubOps {
            status: Some(Ok(status_json(true, Some(1)))),
            backfill: Some(Ok(json!({
                "repo": "demo",
                "gate": {"clean": true, "commits_scanned": 1, "unique_trees": 1, "residual_count": 0, "violations": []},
            }))),
            sync: Some(Ok(json!({
                "repo": "demo",
                "pushed": false,
                "withheld": true,
                "new_commits": 1,
                "gate": {
                    "clean": false, "commits_scanned": 1, "unique_trees": 1, "residual_count": 1,
                    "violations": [{"commit": "ccc", "file": "z.txt", "line": 4, "pattern_kind": "ipv4", "context": "***"}],
                },
            }))),
            ..Default::default()
        };
        let report = run_once_with("demo", &RunnerConfig::default(), &ops).await;
        match report.outcome {
            RunOutcome::GateDirty { residual_count, .. } => assert_eq!(residual_count, 1),
            other => panic!("expected GateDirty, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sync_error_surfaces_as_error_not_panic() {
        let ops = StubOps {
            status: Some(Ok(status_json(true, Some(1)))),
            backfill: Some(Ok(json!({
                "repo": "demo",
                "gate": {"clean": true, "commits_scanned": 1, "unique_trees": 1, "residual_count": 0, "violations": []},
            }))),
            sync: Some(Err(ToolError::NotConfigured("no TERMINUS_MIRROR_AUTHOR_MAP".into()))),
            ..Default::default()
        };
        let report = run_once_with("demo", &RunnerConfig::default(), &ops).await;
        match report.outcome {
            RunOutcome::Error { message } => assert!(message.contains("TERMINUS_MIRROR_AUTHOR_MAP")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_error_surfaces_as_error_not_panic() {
        let ops =
            StubOps { status: Some(Err(ToolError::Execution("git rev-parse failed".into()))), ..Default::default() };
        let report = run_once_with("demo", &RunnerConfig::default(), &ops).await;
        assert!(matches!(report.outcome, RunOutcome::Error { .. }));
        assert_eq!(ops.backfill_calls.load(Ordering::SeqCst), 0);
    }

    /// A per-test-unique temp dir under `TERMINUS_MIRROR_SOURCE_ROOT`, set for
    /// the duration of `f` and cleaned up afterward. `#[serial]` on every
    /// caller of this helper avoids racing `SOURCE_ROOT_ENV` across tests.
    fn with_source_root<R>(setup: impl FnOnce(&Path), f: impl FnOnce() -> R) -> R {
        let dir = std::env::temp_dir()
            .join(format!("mirror-auto-discover-{}", super::super::unique_temp_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        setup(&dir);
        // SAFETY (test-only): callers are `#[serial]`.
        unsafe {
            std::env::set_var(SOURCE_ROOT_ENV, &dir);
        }
        let result = f();
        unsafe {
            std::env::remove_var(SOURCE_ROOT_ENV);
        }
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    /// MIRROR-AUTO opt-out: EVERY repo under the source root is a candidate
    /// by default — `mirror_ready: true` is no longer required. Only an
    /// EXPLICIT `mirror_ready: false` excludes a repo at this layer (the
    /// blacklist is covered separately below).
    #[test]
    #[serial]
    fn list_mirror_candidates_is_opt_out_not_opt_in() {
        let repos = with_source_root(
            |dir| {
                // No .moosenet-pipeline.yaml at all — still a candidate under MIRROR-AUTO.
                std::fs::create_dir_all(dir.join("NoConfigStillCandidate")).unwrap();
                // mirror_ready: true — a candidate (harmless leftover from the old opt-in world).
                std::fs::create_dir_all(dir.join("ExplicitlyTrue")).unwrap();
                std::fs::write(dir.join("ExplicitlyTrue").join(".moosenet-pipeline.yaml"), "mirror_ready: true\n").unwrap();
                // mirror_ready: false — explicitly opted OUT, excluded.
                std::fs::create_dir_all(dir.join("ExplicitlyFalse")).unwrap();
                std::fs::write(dir.join("ExplicitlyFalse").join(".moosenet-pipeline.yaml"), "mirror_ready: false\n").unwrap();
                // some unrelated key, no mirror_ready — still a candidate.
                std::fs::create_dir_all(dir.join("UnrelatedYaml")).unwrap();
                std::fs::write(dir.join("UnrelatedYaml").join(".moosenet-pipeline.yaml"), "other_key: 1\n").unwrap();
            },
            list_mirror_candidates,
        )
        .unwrap();

        assert_eq!(repos, vec!["ExplicitlyTrue".to_string(), "NoConfigStillCandidate".to_string(), "UnrelatedYaml".to_string()]);
    }

    /// The blacklist excludes a repo even though it has no `mirror_ready`
    /// opt-out at all — a purely env-driven exclusion.
    #[test]
    #[serial]
    fn list_mirror_candidates_honors_blacklist() {
        let had = std::env::var(super::super::discovery::BLACKLIST_ENV).ok();
        // SAFETY (test-only): `#[serial]`.
        unsafe {
            std::env::set_var(super::super::discovery::BLACKLIST_ENV, "Blacklisted");
        }
        let repos = with_source_root(
            |dir| {
                std::fs::create_dir_all(dir.join("Blacklisted")).unwrap();
                std::fs::create_dir_all(dir.join("NotBlacklisted")).unwrap();
            },
            list_mirror_candidates,
        )
        .unwrap();
        unsafe {
            match had {
                Some(v) => std::env::set_var(super::super::discovery::BLACKLIST_ENV, v),
                None => std::env::remove_var(super::super::discovery::BLACKLIST_ENV),
            }
        }
        assert_eq!(repos, vec!["NotBlacklisted".to_string()]);
    }

    #[test]
    #[serial]
    fn list_mirror_candidates_no_source_root_is_not_configured() {
        let had = std::env::var(SOURCE_ROOT_ENV).ok();
        // SAFETY (test-only): `#[serial]`.
        unsafe {
            std::env::remove_var(SOURCE_ROOT_ENV);
        }
        let result = list_mirror_candidates();
        unsafe {
            if let Some(v) = had {
                std::env::set_var(SOURCE_ROOT_ENV, v);
            }
        }
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
    }

    // ── MIRROR-AUTO verification gate (codex fix): resolve_and_verify_remote ──
    //
    // These are the security-critical tests: `Verified` is the ONLY resolution
    // that reaches `run_once`/a push (see `GitPublicMirrorRun::execute`), so a
    // `Rejected`/`NoTarget` result is precisely "NOT pushed". Every push path —
    // bulk-discovered, override, and explicit-repo — funnels through this
    // function, so verifying it here proves no unverified target is ever pushed.

    /// A per-name-configurable existence stub: `repo` → exists bool. Any repo
    /// not listed resolves to `false` (absent), matching real discovery.
    struct MapExists {
        answers: std::collections::HashMap<String, bool>,
    }
    impl MapExists {
        fn new(pairs: &[(&str, bool)]) -> Self {
            Self { answers: pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect() }
        }
    }
    #[async_trait]
    impl super::super::discovery::PublicRepoExists for MapExists {
        async fn exists(&self, _owner: &str, repo: &str) -> Result<bool, ToolError> {
            Ok(self.answers.get(repo).copied().unwrap_or(false))
        }
    }

    /// AC (c) happy path — no override, the discovered public repo exists → a
    /// Verified remote (the one thing that leads to a push).
    #[tokio::test]
    #[serial]
    async fn resolve_verified_when_discovered_public_repo_exists() {
        let had = std::env::var(super::super::discovery::GITHUB_ORG_ENV).ok();
        // SAFETY (test-only): `#[serial]`.
        unsafe {
            std::env::remove_var(super::super::discovery::GITHUB_ORG_ENV);
        }
        let verifier = MapExists::new(&[("HasPublicRepo", true)]);
        let res = resolve_and_verify_remote(&verifier, "HasPublicRepo", None).await;
        unsafe {
            if let Some(v) = had {
                std::env::set_var(super::super::discovery::GITHUB_ORG_ENV, v);
            }
        }
        assert_eq!(res, RemoteResolution::Verified("https://github.com/moosenet-io/HasPublicRepo.git".to_string()));
    }

    /// AC (b) explicit-repo / no override, and the public repo does NOT exist →
    /// NoTarget (so execute skips it, never auto-baselines). This is the
    /// explicit-repo-bypass fix: discovery+verification now runs on this path.
    #[tokio::test]
    #[serial]
    async fn resolve_no_target_when_no_override_and_public_repo_absent() {
        let had = std::env::var(super::super::discovery::GITHUB_ORG_ENV).ok();
        // SAFETY (test-only): `#[serial]`.
        unsafe {
            std::env::remove_var(super::super::discovery::GITHUB_ORG_ENV);
        }
        let verifier = MapExists::new(&[("NoPublicRepoYet", false)]);
        let res = resolve_and_verify_remote(&verifier, "NoPublicRepoYet", None).await;
        unsafe {
            if let Some(v) = had {
                std::env::set_var(super::super::discovery::GITHUB_ORG_ENV, v);
            }
        }
        assert_eq!(res, RemoteResolution::NoTarget, "no verified target → never pushed");
    }

    /// AC (a) override BYPASS fix — a call-level `github_remote` override whose
    /// repo_exists=false is REJECTED (not pushed), even though an override was
    /// explicitly supplied.
    #[tokio::test]
    #[serial]
    async fn resolve_rejects_override_to_nonexistent_public_repo() {
        let had = std::env::var(super::super::discovery::GITHUB_ORG_ENV).ok();
        // SAFETY (test-only): `#[serial]`.
        unsafe {
            std::env::remove_var(super::super::discovery::GITHUB_ORG_ENV);
        }
        let verifier = MapExists::new(&[("Ghost", false)]);
        let res = resolve_and_verify_remote(
            &verifier,
            "SomeInternalRepo",
            Some("https://github.com/moosenet-io/Ghost.git"),
        )
        .await;
        unsafe {
            if let Some(v) = had {
                std::env::set_var(super::super::discovery::GITHUB_ORG_ENV, v);
            }
        }
        match res {
            RemoteResolution::Rejected { remote, reason } => {
                assert_eq!(remote, "https://github.com/moosenet-io/Ghost.git");
                assert!(reason.contains("repo_exists=false"), "reason: {reason}");
            }
            other => panic!("an override to a nonexistent repo must be Rejected, got {other:?}"),
        }
    }

    /// THE codex/opus host hole, at the funnel: an override whose owner/repo
    /// WOULD pass repo_exists but whose HOST is not GitHub (evil.example) is
    /// Rejected — never reaches run_once, so internal code is never pushed to
    /// the attacker host. The stub says the repo exists to prove the host
    /// check, not the existence result, is what blocks it.
    #[tokio::test]
    #[serial]
    async fn resolve_rejects_override_on_non_github_host() {
        let had_org = std::env::var(super::super::discovery::GITHUB_ORG_ENV).ok();
        let had_host = std::env::var(super::super::discovery::GITHUB_HOST_ENV).ok();
        // SAFETY (test-only): `#[serial]`. Default org + host (github.com).
        unsafe {
            std::env::remove_var(super::super::discovery::GITHUB_ORG_ENV);
            std::env::remove_var(super::super::discovery::GITHUB_HOST_ENV);
        }
        let verifier = MapExists::new(&[("Terminus", true)]);
        let res =
            resolve_and_verify_remote(&verifier, "Terminus", Some("https://evil.example/moosenet-io/Terminus.git")).await;
        unsafe {
            match had_org {
                Some(v) => std::env::set_var(super::super::discovery::GITHUB_ORG_ENV, v),
                None => std::env::remove_var(super::super::discovery::GITHUB_ORG_ENV),
            }
            match had_host {
                Some(v) => std::env::set_var(super::super::discovery::GITHUB_HOST_ENV, v),
                None => std::env::remove_var(super::super::discovery::GITHUB_HOST_ENV),
            }
        }
        match res {
            RemoteResolution::Rejected { remote, reason } => {
                assert_eq!(remote, "https://evil.example/moosenet-io/Terminus.git");
                assert!(reason.contains("not the configured GitHub host"), "reason: {reason}");
            }
            other => panic!("an override on a non-GitHub host must be Rejected (never pushed), got {other:?}"),
        }
    }

    /// THE codex RFC-3986 userinfo-hijack hole, at the funnel: an override
    /// `https://github.com:<email>/…` (real host evil.example) whose // pii-test-fixture: RFC-3986 userinfo-hijack doc example, email-shaped false positive from "host:<email>"
    /// owner/repo WOULD pass repo_exists is Rejected — never reaches run_once,
    /// so internal code is never pushed to the attacker host.
    #[tokio::test]
    #[serial]
    async fn resolve_rejects_override_with_userinfo_hijack() {
        let had_org = std::env::var(super::super::discovery::GITHUB_ORG_ENV).ok();
        let had_host = std::env::var(super::super::discovery::GITHUB_HOST_ENV).ok();
        // SAFETY (test-only): `#[serial]`. Default org + host (github.com).
        unsafe {
            std::env::remove_var(super::super::discovery::GITHUB_ORG_ENV);
            std::env::remove_var(super::super::discovery::GITHUB_HOST_ENV);
        }
        let verifier = MapExists::new(&[("Terminus", true)]);
        let hijack = "https://github.com:<email>/moosenet-io/Terminus.git"; // pii-test-fixture: userinfo-hijack test fixture, no real PII
        let res = resolve_and_verify_remote(&verifier, "Terminus", Some(hijack)).await;
        unsafe {
            match had_org {
                Some(v) => std::env::set_var(super::super::discovery::GITHUB_ORG_ENV, v),
                None => std::env::remove_var(super::super::discovery::GITHUB_ORG_ENV),
            }
            match had_host {
                Some(v) => std::env::set_var(super::super::discovery::GITHUB_HOST_ENV, v),
                None => std::env::remove_var(super::super::discovery::GITHUB_HOST_ENV),
            }
        }
        match res {
            RemoteResolution::Rejected { remote, .. } => assert_eq!(remote, hijack),
            other => panic!("a userinfo-hijack override must be Rejected (never pushed), got {other:?}"),
        }
    }

    /// AC (c) override happy path — an override that IS verified (host + org +
    /// exists) resolves to Verified and thus DOES push.
    #[tokio::test]
    #[serial]
    async fn resolve_verified_when_override_is_org_matched_and_exists() {
        let had = std::env::var(super::super::discovery::GITHUB_ORG_ENV).ok();
        // SAFETY (test-only): `#[serial]`.
        unsafe {
            std::env::remove_var(super::super::discovery::GITHUB_ORG_ENV);
        }
        let verifier = MapExists::new(&[("Terminus", true)]);
        let res = resolve_and_verify_remote(
            &verifier,
            "Terminus",
            Some("https://github.com/moosenet-io/Terminus.git"),
        )
        .await;
        unsafe {
            if let Some(v) = had {
                std::env::set_var(super::super::discovery::GITHUB_ORG_ENV, v);
            }
        }
        assert_eq!(res, RemoteResolution::Verified("https://github.com/moosenet-io/Terminus.git".to_string()));
    }

    /// A per-repo env override (`TERMINUS_MIRROR_REMOTE_<REPO>`) that fails
    /// verification is Rejected too — the env-override facet of the same gap.
    #[tokio::test]
    #[serial]
    async fn resolve_rejects_env_override_to_nonexistent_public_repo() {
        let had_org = std::env::var(super::super::discovery::GITHUB_ORG_ENV).ok();
        let had_env = std::env::var("TERMINUS_MIRROR_REMOTE_SOMEREPO").ok();
        // SAFETY (test-only): `#[serial]`.
        unsafe {
            std::env::remove_var(super::super::discovery::GITHUB_ORG_ENV);
            std::env::set_var("TERMINUS_MIRROR_REMOTE_SOMEREPO", "https://github.com/moosenet-io/Ghost.git");
        }
        let verifier = MapExists::new(&[("Ghost", false)]);
        let res = resolve_and_verify_remote(&verifier, "SomeRepo", None).await;
        unsafe {
            match had_org {
                Some(v) => std::env::set_var(super::super::discovery::GITHUB_ORG_ENV, v),
                None => std::env::remove_var(super::super::discovery::GITHUB_ORG_ENV),
            }
            match had_env {
                Some(v) => std::env::set_var("TERMINUS_MIRROR_REMOTE_SOMEREPO", v),
                None => std::env::remove_var("TERMINUS_MIRROR_REMOTE_SOMEREPO"),
            }
        }
        assert!(matches!(res, RemoteResolution::Rejected { .. }), "env-override to nonexistent repo must be Rejected: {res:?}");
    }

    #[test]
    fn tool_registers_under_expected_name() {
        let mut reg = ToolRegistry::new();
        reg.register_or_replace(Box::new(GitPublicMirrorRun));
        assert!(reg.contains("git_public_mirror_run"));
    }
}
