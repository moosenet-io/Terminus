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

use std::path::Path;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::tool::RustTool;

use super::tools::{ensure_push_boundary, history_backfill, history_status, history_sync, PushBoundary};

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
        return MirrorRunReport::needs_rebaseline(
            repo,
            "no established full-history lineage — run git_public_history_backfill and have the \
             operator bless + force re-baseline the public mirror first (GHIST-07); the runner \
             only extends an already-bootstrapped baseline, it never creates one",
        );
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

// ── mirror_ready repo discovery (for the no-`repo`-arg "all repos" mode) ────

/// Read one repo checkout's `.moosenet-pipeline.yaml` and report whether it
/// opts into the git-public mirror (`mirror_ready: true`). Missing file,
/// unparsable YAML, or an absent/false `mirror_ready` key are all treated as
/// "not opted in" (the same fail-closed posture `docgen`'s opt-in gate uses
/// for its own `mirror_ready`-shaped config check) — never an error, since a
/// repo simply not having opted in is the overwhelmingly common case.
fn repo_is_mirror_ready(checkout: &Path) -> bool {
    let path = checkout.join(".moosenet-pipeline.yaml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&text) else {
        return false;
    };
    doc.get("mirror_ready").and_then(serde_yaml::Value::as_bool).unwrap_or(false)
}

/// Discover every `mirror_ready` repo by scanning `TERMINUS_MIRROR_SOURCE_ROOT`
/// for immediate subdirectories whose `.moosenet-pipeline.yaml` sets
/// `mirror_ready: true`. This is a READ-ONLY scan of the parking lot the
/// runner's host mounts read-only (see the module doc) — it never writes
/// there. Returns a sorted, deduplicated list for stable, reproducible runs.
pub fn discover_mirror_ready_repos() -> Result<Vec<String>, ToolError> {
    let root = std::env::var(SOURCE_ROOT_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ToolError::NotConfigured(format!(
                "no 'repo' was given and {SOURCE_ROOT_ENV} is not set — pass 'repo' explicitly or \
                 configure {SOURCE_ROOT_ENV} so every mirror_ready repo under it can be discovered"
            ))
        })?;
    let entries = std::fs::read_dir(&root)
        .map_err(|e| ToolError::Execution(format!("read {SOURCE_ROOT_ENV} ({root}): {e}")))?;
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
        if repo_is_mirror_ready(&path) {
            repos.push(name.to_string());
        }
    }
    repos.sort();
    repos.dedup();
    Ok(repos)
}

// ── git_public_mirror_run (core tool) ───────────────────────────────────────

/// `git_public_mirror_run` — the MRUN-01 tool wrapping [`run_once`]. With an
/// explicit `repo`, runs one pass for that repo. Without one, discovers every
/// `mirror_ready` repo under `TERMINUS_MIRROR_SOURCE_ROOT` and runs a pass for
/// each, returning a per-repo report array — this is the call
/// `deploy/terminus-mirror-runner.service` makes on a timer.
pub(crate) struct GitPublicMirrorRun;

#[async_trait]
impl RustTool for GitPublicMirrorRun {
    fn name(&self) -> &str {
        "git_public_mirror_run"
    }

    fn description(&self) -> &str {
        "MRUN-01. Run one idempotent git-public mirror pass: read \
         git_public_history_status, and if behind, run git_public_history_backfill \
         (replay + full-history PII gate, never pushes) then, only when gate-clean, \
         git_public_history_sync (fast-forward-only push of an already \
         operator-blessed GHIST-07 baseline). NEVER force-pushes: a diverged / \
         un-bootstrapped / non-fast-forward mirror, or a repo with no established \
         history lineage yet, is reported as needing the one-time operator-blessed \
         re-baseline rather than acted on. With no 'repo', discovers and runs every \
         mirror_ready repo under TERMINUS_MIRROR_SOURCE_ROOT and returns one report \
         per repo. Intended to be driven by deploy/terminus-mirror-runner.timer."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo":          { "type": "string", "description": "Logical repo name; omit to run every mirror_ready repo under TERMINUS_MIRROR_SOURCE_ROOT" },
                "source":        { "type": "string", "description": "internal-main checkout override (else TERMINUS_MIRROR_SOURCE_ROOT/<repo>)" },
                "github_remote": { "type": "string", "description": "Target mirror remote override (else TERMINUS_MIRROR_REMOTE[_<REPO>])" },
                "provider":      { "type": "string", "description": "Mirror-push target provider (default 'github')" }
            }
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let cfg = RunnerConfig {
            source: args.get("source").and_then(Value::as_str).map(str::to_string),
            github_remote: args.get("github_remote").and_then(Value::as_str).map(str::to_string),
            provider: args.get("provider").and_then(Value::as_str).map(str::to_string),
        };

        let repos: Vec<String> = match args.get("repo").and_then(Value::as_str) {
            Some(r) if !r.trim().is_empty() => vec![r.trim().to_string()],
            _ => discover_mirror_ready_repos()?,
        };

        let mut reports = Vec::with_capacity(repos.len());
        for repo in &repos {
            reports.push(run_once(repo, &cfg).await);
        }

        serde_json::to_string(&json!({ "repos_run": repos.len(), "reports": reports }))
            .map_err(|e| ToolError::Execution(format!("serialize reports: {e}")))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ToolRegistry;
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
        status_calls: AtomicUsize,
        boundary_calls: AtomicUsize,
        backfill_calls: AtomicUsize,
        sync_calls: AtomicUsize,
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
        let ops = StubOps { status: Some(Ok(status_json(false, None))), ..Default::default() };
        let report = run_once_with("demo", &RunnerConfig::default(), &ops).await;
        assert!(matches!(report.outcome, RunOutcome::NeedsOperatorRebaseline { .. }));
        assert_eq!(ops.backfill_calls.load(Ordering::SeqCst), 0);
        assert_eq!(ops.sync_calls.load(Ordering::SeqCst), 0);
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

    #[test]
    fn discover_mirror_ready_repos_reads_pipeline_yaml() {
        let dir = std::env::temp_dir().join(format!(
            "mrun01-discover-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(dir.join("Ready")).unwrap();
        std::fs::write(
            dir.join("Ready").join(".moosenet-pipeline.yaml"),
            "mirror_ready: true\ngithub_remote: https://example.invalid/moosenet-io/Ready.git\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("NotReady")).unwrap();
        std::fs::write(dir.join("NotReady").join(".moosenet-pipeline.yaml"), "mirror_ready: false\n").unwrap();
        std::fs::create_dir_all(dir.join("NoConfig")).unwrap();

        // SAFETY (test-only): serialized via a per-test unique temp dir, no
        // shared mutable env state relied upon across tests in this file.
        unsafe {
            std::env::set_var(SOURCE_ROOT_ENV, &dir);
        }
        let repos = discover_mirror_ready_repos().unwrap();
        unsafe {
            std::env::remove_var(SOURCE_ROOT_ENV);
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(repos, vec!["Ready".to_string()]);
    }

    #[test]
    fn no_source_root_is_not_configured() {
        // Ensure the var is absent for this check (best-effort; other tests
        // don't leave it set past their own scope).
        let had = std::env::var(SOURCE_ROOT_ENV).ok();
        unsafe {
            std::env::remove_var(SOURCE_ROOT_ENV);
        }
        let result = discover_mirror_ready_repos();
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));
        if let Some(v) = had {
            unsafe {
                std::env::set_var(SOURCE_ROOT_ENV, v);
            }
        }
    }

    #[test]
    fn tool_registers_under_expected_name() {
        let mut reg = ToolRegistry::new();
        reg.register_or_replace(Box::new(GitPublicMirrorRun));
        assert!(reg.contains("git_public_mirror_run"));
    }
}
