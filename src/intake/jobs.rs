//! BLD-ASYNC (TERM #421, intake half): an in-process, non-persistent job
//! registry backing the async `model_intake_fleet` surface.
//!
//! `model_intake_fleet` blocks on the whole fleet sweep, which routinely
//! exceeds the loopback MCP forward timeout — the CALL fails ("primary
//! unreachable, timed out after 900s") even though the sweep continues (or
//! aborts) server-side with no way for the caller to observe which. Rather
//! than persist job state to Postgres (a bigger surface for v1), this is a
//! single in-process registry: good enough because the fleet sweep and the
//! MCP process serving `model_intake_status`-style polls are the same
//! process — a process restart loses in-flight job state, which is
//! acceptable for v1 (the sweep itself is safely re-runnable).
//!
//! Mirrors the SHAPE of the compiler's async job pattern
//! (`compiler_build(wait=false)` + `compiler_progress`) without borrowing its
//! Redis-backed queue machinery, which is overkill for a single-process,
//! single-fleet-sweep-at-a-time workload.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};
use serde::Serialize;

/// Lifecycle state of an async fleet-intake job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Completed => "completed",
            JobStatus::Failed => "failed",
        }
    }
}

/// In-flight progress within a running sweep: how many models are done out of
/// the total, and which model/suite-set is currently in flight (both `None`
/// before the first model starts or after the last one finishes).
#[derive(Debug, Clone, Default, Serialize)]
pub struct JobProgress {
    pub models_done: usize,
    pub models_total: usize,
    pub current_model: Option<String>,
    pub current_suites: Option<String>,
}

/// Full state of one async fleet-intake job.
#[derive(Debug, Clone, Serialize)]
pub struct JobState {
    pub job_id: String,
    pub status: JobStatus,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub progress: JobProgress,
    /// Final human-readable summary (the same text the synchronous path
    /// returns), set on `Completed`.
    pub summary: Option<String>,
    /// Error message, set on `Failed`.
    pub error: Option<String>,
}

impl JobState {
    fn new(job_id: String) -> Self {
        let now = Utc::now();
        JobState {
            job_id,
            status: JobStatus::Queued,
            started_at: now,
            updated_at: now,
            progress: JobProgress::default(),
            summary: None,
            error: None,
        }
    }
}

type Registry = Mutex<HashMap<String, JobState>>;

/// Upper bound on retained jobs. When a new job is created past this many,
/// the OLDEST terminal (completed/failed) jobs are evicted first so the
/// registry never grows unbounded over a long-lived process. Active
/// (queued/running) jobs are NEVER evicted. Generous — a fleet sweep is an
/// occasional, operator-driven event, so this holds a long tail of history.
const MAX_JOBS: usize = 50;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A job is "active" (occupying the single fleet-sweep slot) while it is
/// queued or running. Completed/failed jobs are inert history.
fn is_active(status: JobStatus) -> bool {
    matches!(status, JobStatus::Queued | JobStatus::Running)
}

/// Evict oldest terminal jobs (in place, under the caller's lock) until the
/// map is back under [`MAX_JOBS`]. Only completed/failed jobs are candidates;
/// active jobs are retained regardless of count (there is at most one at a
/// time anyway, per [`try_start_job`]).
fn evict_over_cap(map: &mut HashMap<String, JobState>) {
    if map.len() < MAX_JOBS {
        return;
    }
    let mut terminal: Vec<(String, DateTime<Utc>)> = map
        .iter()
        .filter(|(_, s)| !is_active(s.status))
        .map(|(id, s)| (id.clone(), s.started_at))
        .collect();
    terminal.sort_by(|a, b| a.1.cmp(&b.1)); // oldest first
    // Trim enough terminal jobs to leave room for the incoming one.
    let mut to_remove = map.len().saturating_sub(MAX_JOBS) + 1;
    for (id, _) in terminal {
        if to_remove == 0 {
            break;
        }
        map.remove(&id);
        to_remove -= 1;
    }
}

/// Atomically claim the single fleet-sweep slot and create a fresh `Queued`
/// job. Returns `Ok(new_job_id)` when no other sweep is in flight, or
/// `Err(existing_active_job_id)` when a job is already queued/running — the
/// caller turns that into a clear "already running, poll <id>" response.
///
/// The active-check and the insert happen under a SINGLE lock hold, so two
/// concurrent async submits can never both win the slot (the review's
/// concurrent-sweep concern: two overlapping fleet sweeps would contend for
/// the GPU). Also opportunistically evicts old terminal jobs to bound the
/// registry. Use this (not [`create_job`]) for the async fleet path.
pub fn try_start_job() -> Result<String, String> {
    let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(active) = map.values().find(|s| is_active(s.status)) {
        return Err(active.job_id.clone());
    }
    evict_over_cap(&mut map);
    let job_id = uuid::Uuid::new_v4().to_string();
    map.insert(job_id.clone(), JobState::new(job_id.clone()));
    Ok(job_id)
}

/// Create a new job in `Queued` state and return its id, with NO
/// concurrency guard (used by tests and any non-fleet caller). The async
/// fleet path uses [`try_start_job`] instead so overlapping sweeps are
/// rejected. Still bounds the registry via [`evict_over_cap`].
pub fn create_job() -> String {
    let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
    evict_over_cap(&mut map);
    let job_id = uuid::Uuid::new_v4().to_string();
    map.insert(job_id.clone(), JobState::new(job_id.clone()));
    job_id
}

/// Mark a job `Running` (called once the spawned task actually starts).
pub fn mark_running(job_id: &str) {
    if let Some(s) = registry().lock().unwrap_or_else(|e| e.into_inner()).get_mut(job_id) {
        s.status = JobStatus::Running;
        s.updated_at = Utc::now();
    }
}

/// Update per-model progress on a running job. Safe to call even if the job
/// id is unknown (e.g. it was evicted) — a no-op in that case rather than a
/// panic, since progress reporting must never crash the sweep itself.
pub fn update_progress(job_id: &str, done: usize, total: usize, model: Option<&str>, suites: Option<&str>) {
    if let Some(s) = registry().lock().unwrap_or_else(|e| e.into_inner()).get_mut(job_id) {
        s.progress.models_done = done;
        s.progress.models_total = total;
        s.progress.current_model = model.map(String::from);
        s.progress.current_suites = suites.map(String::from);
        s.updated_at = Utc::now();
    }
}

/// Mark a job `Completed` with its final summary text.
pub fn mark_completed(job_id: &str, summary: String) {
    if let Some(s) = registry().lock().unwrap_or_else(|e| e.into_inner()).get_mut(job_id) {
        s.status = JobStatus::Completed;
        s.summary = Some(summary);
        s.updated_at = Utc::now();
    }
}

/// Mark a job `Failed` with an error message.
pub fn mark_failed(job_id: &str, error: String) {
    if let Some(s) = registry().lock().unwrap_or_else(|e| e.into_inner()).get_mut(job_id) {
        s.status = JobStatus::Failed;
        s.error = Some(error);
        s.updated_at = Utc::now();
    }
}

/// Look up one job's current state.
pub fn get_job(job_id: &str) -> Option<JobState> {
    registry().lock().unwrap_or_else(|e| e.into_inner()).get(job_id).cloned()
}

/// List recent jobs (most-recently-started first), capped at `limit`.
pub fn list_jobs(limit: usize) -> Vec<JobState> {
    let map = registry().lock().unwrap_or_else(|e| e.into_inner());
    let mut jobs: Vec<JobState> = map.values().cloned().collect();
    jobs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    jobs.truncate(limit);
    jobs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn job_lifecycle_queued_running_completed() {
        let id = create_job();
        let s = get_job(&id).expect("job present after create");
        assert_eq!(s.status, JobStatus::Queued);
        assert_eq!(s.progress.models_done, 0);
        assert!(s.summary.is_none());
        assert!(s.error.is_none());

        mark_running(&id);
        let s = get_job(&id).unwrap();
        assert_eq!(s.status, JobStatus::Running);

        update_progress(&id, 1, 3, Some("gpt-oss:20b"), Some("context+agent"));
        let s = get_job(&id).unwrap();
        assert_eq!(s.progress.models_done, 1);
        assert_eq!(s.progress.models_total, 3);
        assert_eq!(s.progress.current_model.as_deref(), Some("gpt-oss:20b"));
        assert_eq!(s.progress.current_suites.as_deref(), Some("context+agent"));

        mark_completed(&id, "Fleet intake complete: 3 model(s)".to_string());
        let s = get_job(&id).unwrap();
        assert_eq!(s.status, JobStatus::Completed);
        assert_eq!(s.summary.as_deref(), Some("Fleet intake complete: 3 model(s)"));
    }

    #[test]
    #[serial_test::serial]
    fn job_lifecycle_failed_path() {
        let id = create_job();
        mark_running(&id);
        mark_failed(&id, "no models to profile".to_string());
        let s = get_job(&id).unwrap();
        assert_eq!(s.status, JobStatus::Failed);
        assert_eq!(s.error.as_deref(), Some("no models to profile"));
        // A failed job carries no summary.
        assert!(s.summary.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn unknown_job_id_returns_none_and_updates_are_noops() {
        assert!(get_job("does-not-exist").is_none());
        // These must not panic even though the id is unknown.
        mark_running("does-not-exist");
        update_progress("does-not-exist", 1, 1, None, None);
        mark_completed("does-not-exist", "x".into());
        mark_failed("does-not-exist", "x".into());
    }

    #[test]
    #[serial_test::serial]
    fn create_job_ids_are_unique() {
        let a = create_job();
        let b = create_job();
        assert_ne!(a, b);
    }

    #[test]
    #[serial_test::serial]
    fn list_jobs_orders_most_recent_first_and_respects_limit() {
        let a = create_job();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = create_job();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let c = create_job();

        // Filter to just OUR three ids — the global registry is shared across
        // the (serialized) suite, so residue from a prior test may sit among
        // these; their presence must not affect the relative order of a/b/c.
        // All three are active (queued), so eviction never removes them.
        let all = list_jobs(usize::MAX);
        let mine: Vec<&String> = all
            .iter()
            .map(|j| &j.job_id)
            .filter(|id| **id == a || **id == b || **id == c)
            .collect();
        // Most-recently-started first among our own jobs.
        assert_eq!(mine, vec![&c, &b, &a]);
        // The limit caps the returned count.
        assert_eq!(list_jobs(2).len(), 2);
        assert!(get_job(&a).is_some());
    }

    #[test]
    #[serial_test::serial]
    fn job_status_as_str_is_stable() {
        assert_eq!(JobStatus::Queued.as_str(), "queued");
        assert_eq!(JobStatus::Running.as_str(), "running");
        assert_eq!(JobStatus::Completed.as_str(), "completed");
        assert_eq!(JobStatus::Failed.as_str(), "failed");
    }

    /// Drain (mark completed) every currently-active job so a guard/cap test can
    /// start from a slot-free registry despite the shared global singleton. The
    /// jobs it touches belong to already-finished tests (this suite's
    /// slot-sensitive tests are serialized), so completing them is harmless.
    fn drain_active() {
        for j in list_jobs(usize::MAX) {
            if is_active(j.status) {
                mark_completed(&j.job_id, "test-drain".into());
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn try_start_job_rejects_a_second_concurrent_sweep() {
        drain_active();
        // Claim the single sweep slot (drain-retry any job a parallel non-serial
        // test raced in between the drain and here).
        let id1 = {
            let mut got = None;
            for _ in 0..100 {
                match try_start_job() {
                    Ok(id) => {
                        got = Some(id);
                        break;
                    }
                    Err(active) => mark_completed(&active, "test-drain".into()),
                }
            }
            got.expect("claimed the sweep slot")
        };
        mark_running(&id1);

        // The guard invariant: while id1 is active, a second submit is REJECTED
        // (never spawns a second, GPU-contending sweep) and names an in-flight
        // job. id1 is definitively active, so this is deterministic.
        match try_start_job() {
            Ok(other) => panic!("expected rejection while {id1} active, got new job {other}"),
            Err(active) => assert!(is_active(get_job(&active).unwrap().status)),
        }

        // Freeing the slot (completing id1) lets a fresh sweep start again.
        mark_completed(&id1, "done".into());
        let id2 = {
            let mut got = None;
            for _ in 0..100 {
                match try_start_job() {
                    Ok(id) => {
                        got = Some(id);
                        break;
                    }
                    Err(active) => mark_completed(&active, "test-drain".into()),
                }
            }
            got.expect("slot re-openable after completion")
        };
        assert_ne!(id1, id2);
        mark_completed(&id2, "cleanup".into());
    }

    #[test]
    #[serial_test::serial]
    fn registry_is_capped_and_never_evicts_an_active_job() {
        drain_active();
        // An active job must survive eviction pressure.
        let active = {
            let mut got = None;
            for _ in 0..100 {
                match try_start_job() {
                    Ok(id) => {
                        got = Some(id);
                        break;
                    }
                    Err(a) => mark_completed(&a, "test-drain".into()),
                }
            }
            got.expect("claimed slot")
        };
        mark_running(&active);

        // Push well past the cap with terminal jobs.
        for _ in 0..(MAX_JOBS + 20) {
            let id = create_job();
            mark_completed(&id, "bulk".into());
        }

        // The registry is bounded (terminal jobs evicted oldest-first)...
        assert!(list_jobs(usize::MAX).len() <= MAX_JOBS, "registry stays within the cap");
        // ...but the active job is NEVER evicted.
        assert!(get_job(&active).is_some(), "active job survives eviction pressure");
        assert!(is_active(get_job(&active).unwrap().status));

        mark_completed(&active, "cleanup".into());
    }
}
