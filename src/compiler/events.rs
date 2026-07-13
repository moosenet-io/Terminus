//! BLD-19 — the compiler progress/events bus: a live build-status surface.
//!
//! `compiler_build` (BLD-05) is the single build door but, until now, a client
//! only learned the outcome when the tool call *returned* — a spinner, never a
//! progress bar. This module is a first-class **progress surface**: every build
//! carries a stable `request_id`, each lifecycle stage emits a typed event into a
//! per-request ring buffer + a live broadcast channel, and `compiler_progress`
//! lets a client (the fleet GUI BLD-15, a requesting agent, the Harmony adapter)
//! read the current snapshot AND long-poll for new events as they occur.
//!
//! ## The model
//! One [`ProgressEvent`] per stage transition (plus throttled log-tail lines
//! during the build). Stages, in order:
//!   `queued → scheduled → [relaying (remote only)] → building{step,total} →
//!    publishing → deployed | failed | rolled_back`
//!
//! `relaying` is a REMOTE-path stage: it means "rsync the source to the heavy
//! build host". A LOCAL (primary, in-place) build has nothing to relay, so it
//! legitimately goes `scheduled → building` directly — a local stream without a
//! `relaying` event is valid and expected, not a gap.
//!
//! ## Stage TRANSITIONS vs. progress UPDATES
//! Two kinds of event share the stream. A STAGE TRANSITION (each of
//! queued/scheduled/relaying/building-STARTED/publishing/published/failed) is
//! always emitted and retained exactly once — even a build whose cargo output has
//! no parseable `{step,total}` line still shows a `building` (started) event. A
//! progress UPDATE is an intermediate building tick carrying `{step,total}`
//! (streamed from cargo's `N/M`); only those are throttled — a duplicate/unchanged
//! step is coalesced. The throttle NEVER drops a stage transition.
//!
//! `compiler_build`'s own scope ends at publish, so it emits the terminal
//! [`Stage::Published`] (with the artifact sha) on success and [`Stage::Failed`]
//! (with a sanitized error tail) on failure. [`Stage::Deployed`] and
//! [`Stage::RolledBack`] are reserved for the downstream updater stage so the
//! model spans the whole request lifecycle the GUI renders — they are valid
//! events any pipeline stage may emit against the same `request_id`.
//!
//! ## Seam with `compiler_status` (BLD-08)
//! `compiler_status` is a POINT-IN-TIME aggregate (queue + store `current`
//! pointers + a module×host deployed-sha matrix). This is the LIVE per-request
//! event STREAM. They do not overlap: status answers "what is deployed where
//! right now", progress answers "how is *this* build going, second by second".
//!
//! ## Store & bounds (ephemeral, fail-open — like the BLD-20 admission queue)
//! The event store is IN-PROCESS: a per-build ring buffer + a broadcast channel.
//! It is intentionally NOT durable — progress is ephemeral, and BLD-08
//! `compiler_status` remains the point-in-time truth if the process restarts. The
//! bounds keep it from ever growing without limit:
//! - Per-build ring buffer capped at [`max_events`](ProgressBus::max_events); the
//!   oldest events fall off (a late subscriber still gets the current stage + the
//!   recent tail).
//! - At most [`max_builds`](ProgressBus::max_builds) tracked; the
//!   least-recently-updated build is evicted when the map is full, and any build
//!   idle past the TTL is swept on the next write AND enforced on every READ
//!   (`snapshot`/`poll`), so a quiet process still returns `not_found` for an
//!   expired id (the stale track is lazily evicted on that read).
//! All three bounds are env-tunable numeric knobs (not infra literals — S1).
//!
//! ## Secret discipline (S6/S7)
//! Log-tail lines are secret-sanitized by the *emitter* (`compiler_build` runs
//! every captured line through its existing redaction set) BEFORE they enter the
//! bus, so a secret never leaves the process through the stream. The bus itself
//! stores only stage/timing/step data and already-redacted text — no secrets, no
//! infra literals.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::broadcast;

/// Env knob: max events retained per build (ring-buffer depth). Default 256.
const ENV_MAX_EVENTS: &str = "COMPILER_PROGRESS_MAX_EVENTS";
/// Env knob: max concurrently-tracked builds. Default 64.
const ENV_MAX_BUILDS: &str = "COMPILER_PROGRESS_MAX_BUILDS";
/// Env knob: idle TTL (seconds) after which a build track is swept. Default 3600.
const ENV_TTL_SECS: &str = "COMPILER_PROGRESS_TTL_SECS";

const DEFAULT_MAX_EVENTS: usize = 256;
const DEFAULT_MAX_BUILDS: usize = 64;
const DEFAULT_TTL_SECS: u64 = 3600;
/// Broadcast channel depth per build. A slow long-poller that lags past this
/// just observes a `Lagged` skip and re-reads the snapshot — the ring buffer is
/// the durable record, the channel is only the live wakeup.
const BROADCAST_DEPTH: usize = 64;
/// Max stored length of an event `message` (a few KB): one huge error/log line
/// must not escape the ring/capacity memory bound. Longer messages are truncated
/// with a marker. A char-boundary-safe cut.
const MAX_MESSAGE_LEN: usize = 4096;
/// Max stored length of a `request_id` (it is a map key + echoed in every event);
/// bound it so a pathological id can't blow the memory bound either.
const MAX_REQUEST_ID_LEN: usize = 128;

/// Truncate `s` to at most `max` BYTES on a char boundary; when cut, append
/// `marker` (so a truncated message is noted). Returns `s` unchanged if it fits.
fn clamp_str(s: &str, max: usize, marker: &str) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{marker}", &s[..end])
}

/// A `request_id` clamped to the bounded key length (no marker — it stays a plain
/// key). Applied consistently on write (emit) and read (snapshot/subscribe) so a
/// clamped key always matches itself.
fn clamp_request_id(id: &str) -> String {
    clamp_str(id, MAX_REQUEST_ID_LEN, "")
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

/// Milliseconds since the Unix epoch (good enough for event timestamps/TTL).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A build lifecycle stage. Ordered as emitted; the terminal stages
/// ([`is_terminal`](Stage::is_terminal)) close the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Queued,
    Scheduled,
    /// Staging source to the heavy build host. REMOTE path only — a local
    /// (in-place) build has nothing to relay and skips straight to `Building`.
    Relaying,
    Building,
    Publishing,
    /// Terminal success for `compiler_build` (artifact published, sha known).
    Published,
    /// Terminal failure (a sanitized error tail is attached).
    Failed,
    /// Terminal success reserved for the downstream updater stage.
    Deployed,
    /// Terminal rollback reserved for the downstream updater stage.
    RolledBack,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Queued => "queued",
            Stage::Scheduled => "scheduled",
            Stage::Relaying => "relaying",
            Stage::Building => "building",
            Stage::Publishing => "publishing",
            Stage::Published => "published",
            Stage::Failed => "failed",
            Stage::Deployed => "deployed",
            Stage::RolledBack => "rolled_back",
        }
    }

    /// Whether this stage closes the stream (no further events expected).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Stage::Published | Stage::Failed | Stage::Deployed | Stage::RolledBack
        )
    }
}

/// One event in a build's progress stream. Cheap to clone; broadcast by value.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgressEvent {
    /// The build this event belongs to.
    pub request_id: String,
    /// Monotonic per-build cursor (1-based); a client polls "events since seq".
    pub seq: u64,
    /// Unix-epoch milliseconds when the event was emitted.
    pub ts_ms: u64,
    pub stage: Stage,
    /// Build progress `{step,total}` (crate n of m), when known (building stage).
    pub step: Option<u32>,
    pub total: Option<u32>,
    /// A short, ALREADY-SANITIZED note or log-tail line (never a raw secret).
    pub message: Option<String>,
    /// On [`Stage::Published`]/[`Stage::Deployed`]: the artifact sha256.
    pub sha: Option<String>,
}

impl ProgressEvent {
    fn to_json(&self) -> Value {
        json!({
            "request_id": self.request_id,
            "seq": self.seq,
            "ts_ms": self.ts_ms,
            "stage": self.stage.as_str(),
            "step": self.step,
            "total": self.total,
            "message": self.message,
            "sha": self.sha,
        })
    }
}

/// A builder for one event, so the emit call sites stay terse and typed.
pub struct Emit {
    stage: Stage,
    step: Option<u32>,
    total: Option<u32>,
    message: Option<String>,
    sha: Option<String>,
}

impl Emit {
    pub fn stage(stage: Stage) -> Self {
        Self {
            stage,
            step: None,
            total: None,
            message: None,
            sha: None,
        }
    }
    pub fn progress(mut self, step: u32, total: u32) -> Self {
        self.step = Some(step);
        self.total = Some(total);
        self
    }
    pub fn message(mut self, msg: impl Into<String>) -> Self {
        let m = msg.into();
        if !m.is_empty() {
            self.message = Some(m);
        }
        self
    }
    pub fn sha(mut self, sha: impl Into<String>) -> Self {
        let s = sha.into();
        if !s.is_empty() {
            self.sha = Some(s);
        }
        self
    }
}

/// A single build's retained history + live broadcast handle.
struct Track {
    events: Vec<ProgressEvent>,
    next_seq: u64,
    created_ms: u64,
    updated_ms: u64,
    /// Monotonic "last-touched" ordinal from the bus's global touch counter,
    /// bumped on every emit. This — NOT the wall clock — is the LRU key, so two
    /// updates in the same `SystemTime` tick still have a strict, deterministic
    /// order and eviction can never tie (fixes the flaky eviction test).
    last_touched: u64,
    /// Last emitted `{step,total}`, so building-progress lines that don't advance
    /// the step are dropped (throttle — never a flood of identical events).
    last_step: Option<(u32, u32)>,
    terminal: Option<Stage>,
    tx: broadcast::Sender<ProgressEvent>,
}

impl Track {
    fn new(now: u64, touched: u64, max_events: usize) -> Self {
        let depth = BROADCAST_DEPTH.max(max_events.min(1024));
        let (tx, _rx) = broadcast::channel(depth);
        Self {
            events: Vec::new(),
            next_seq: 1,
            created_ms: now,
            updated_ms: now,
            last_touched: touched,
            last_step: None,
            terminal: None,
            tx,
        }
    }

    fn snapshot_stage(&self) -> Stage {
        self.events.last().map(|e| e.stage).unwrap_or(Stage::Queued)
    }
}

/// The process-global progress bus. Constructed once (lazily) with the tunable
/// bounds; all `compiler_*` progress goes through the single instance.
pub struct ProgressBus {
    inner: Mutex<HashMap<String, Track>>,
    /// Monotonic touch counter: every emit assigns the next value to the touched
    /// build's `last_touched`, giving a strict LRU order independent of wall-clock
    /// resolution (so eviction is deterministic even under test parallelism).
    touch: AtomicU64,
    max_events: usize,
    max_builds: usize,
    ttl_ms: u64,
}

static BUS: OnceLock<ProgressBus> = OnceLock::new();

/// The process-global bus.
pub fn bus() -> &'static ProgressBus {
    BUS.get_or_init(ProgressBus::from_env)
}

/// A snapshot returned to a client: the current stage + a bounded recent tail.
pub struct Snapshot {
    pub request_id: String,
    pub stage: Stage,
    pub terminal: bool,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub last_seq: u64,
    /// Latest `{step,total}` seen (for a progress bar), if any.
    pub step: Option<u32>,
    pub total: Option<u32>,
    /// Events with `seq > since` (or the whole retained tail when `since == 0`).
    pub events: Vec<ProgressEvent>,
}

impl Snapshot {
    pub fn to_json(&self) -> Value {
        json!({
            "request_id": self.request_id,
            "stage": self.stage.as_str(),
            "terminal": self.terminal,
            "created_ms": self.created_ms,
            "updated_ms": self.updated_ms,
            "last_seq": self.last_seq,
            "step": self.step,
            "total": self.total,
            "events": self.events.iter().map(ProgressEvent::to_json).collect::<Vec<_>>(),
        })
    }
}

impl ProgressBus {
    fn from_env() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            touch: AtomicU64::new(0),
            max_events: env_usize(ENV_MAX_EVENTS, DEFAULT_MAX_EVENTS),
            max_builds: env_usize(ENV_MAX_BUILDS, DEFAULT_MAX_BUILDS),
            ttl_ms: env_u64(ENV_TTL_SECS, DEFAULT_TTL_SECS).saturating_mul(1000),
        }
    }

    /// Test/explicit constructor with fixed bounds (no env). `ttl_ms == 0`
    /// disables the idle sweep.
    pub fn with_bounds(max_events: usize, max_builds: usize, ttl_ms: u64) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            touch: AtomicU64::new(0),
            max_events: max_events.max(1),
            max_builds: max_builds.max(1),
            ttl_ms,
        }
    }

    pub fn max_events(&self) -> usize {
        self.max_events
    }
    pub fn max_builds(&self) -> usize {
        self.max_builds
    }

    /// Sweep builds idle past the TTL. Caller holds the lock. `now` is `now_ms()`.
    fn sweep_locked(map: &mut HashMap<String, Track>, ttl_ms: u64, now: u64) {
        if ttl_ms == 0 {
            return;
        }
        map.retain(|_, t| now.saturating_sub(t.updated_ms) <= ttl_ms);
    }

    /// Evict the least-recently-updated build while at capacity (never the build
    /// we are about to write to). Caller holds the lock. Ordering is by the
    /// monotonic `last_touched` ordinal (strict, tie-free), NOT the wall clock.
    fn evict_if_full_locked(map: &mut HashMap<String, Track>, max_builds: usize, keep: &str) {
        while map.len() >= max_builds && !map.contains_key(keep) {
            let victim = map
                .iter()
                .min_by_key(|(_, t)| t.last_touched)
                .map(|(k, _)| k.clone());
            match victim {
                Some(v) => {
                    map.remove(&v);
                }
                None => break,
            }
        }
    }

    /// Emit an event for `request_id`, creating the track if new. Returns the
    /// assigned seq. Applies TTL sweep + capacity eviction + ring-buffer bound,
    /// and (for the building stage) throttles unchanged `{step,total}`.
    pub fn emit(&self, request_id: &str, e: Emit) -> u64 {
        let now = now_ms();
        // Bound the id length (map key + echoed in every event) so a pathological
        // id can't escape the memory bound; clamp identically on read.
        let rid = clamp_request_id(request_id);
        // A strictly-increasing LRU ordinal for THIS emit — assigned to the
        // touched build below so eviction order never depends on wall-clock
        // resolution (two emits in the same tick still order strictly).
        let touched = self.touch.fetch_add(1, Ordering::Relaxed);
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());

        Self::sweep_locked(&mut map, self.ttl_ms, now);
        Self::evict_if_full_locked(&mut map, self.max_builds, &rid);

        let max_events = self.max_events;
        let track = map
            .entry(rid.clone())
            .or_insert_with(|| Track::new(now, touched, max_events));

        // TERMINAL INTEGRITY: once a stream is terminal (published/failed/deployed/
        // rolled_back) it is CLOSED — a later event is ignored, so a terminal
        // snapshot can never report an inconsistent later stage (e.g. building
        // after published). Deterministic close.
        if track.terminal.is_some() {
            return track.next_seq.saturating_sub(1);
        }

        // Throttle applies ONLY to intermediate `{step,total}` progress UPDATES —
        // the building ticks the tap streams from cargo's `N/M` output — NEVER to a
        // STAGE TRANSITION. A stage transition (queued/scheduled/relaying/building-
        // STARTED/publishing/published/failed) is always emitted + retained exactly
        // once per transition, so even a build whose cargo emits no parseable `N/M`
        // line still shows `building` (started) → `publishing`.
        //
        // A building progress UPDATE is a building event that carries `{step,total}`
        // and no message; only its duplicate (same step) is coalesced. A contentless
        // `building` (the intentional "build started" transition) falls straight
        // through and is retained.
        let is_progress_update = e.stage == Stage::Building
            && e.message.is_none()
            && e.step.is_some()
            && e.total.is_some();
        if is_progress_update {
            let pair = (e.step.unwrap(), e.total.unwrap());
            if track.last_step == Some(pair) {
                // Duplicate/unchanged progress tick — coalesce (do not retain).
                return track.next_seq.saturating_sub(1);
            }
            track.last_step = pair.into();
        } else if e.stage == Stage::Building {
            // A building event that also carries `{step,total}` (e.g. a started
            // transition annotated with progress) still updates the latest step.
            if let (Some(step), Some(total)) = (e.step, e.total) {
                track.last_step = Some((step, total));
            }
        }

        let seq = track.next_seq;
        track.next_seq += 1;
        track.updated_ms = now;
        // Mark this build most-recently-used (a newly-created track already holds
        // this same ordinal; an existing one advances to it).
        track.last_touched = touched;
        if e.stage.is_terminal() {
            track.terminal = Some(e.stage);
        }

        // Bound the stored message so one huge error/log line can't escape the
        // ring/capacity memory bound; a truncated message is marked.
        let message = e
            .message
            .map(|m| clamp_str(&m, MAX_MESSAGE_LEN, "…[truncated]"));
        let event = ProgressEvent {
            request_id: rid.clone(),
            seq,
            ts_ms: now,
            stage: e.stage,
            step: e.step,
            total: e.total,
            message,
            sha: e.sha,
        };
        track.events.push(event.clone());
        // Ring-buffer bound: drop the oldest beyond the cap (front of the Vec).
        let overflow = track.events.len().saturating_sub(max_events);
        if overflow > 0 {
            track.events.drain(0..overflow);
        }
        // Live wakeup; ignore "no subscribers" — the ring buffer is the record.
        let _ = track.tx.send(event);
        seq
    }

    /// Whether a build whose last wall-clock activity was `updated_ms` is expired
    /// under `ttl_ms` at `now`. `ttl_ms == 0` disables expiry. Uses the WALL CLOCK
    /// (the same field the emit-side sweep uses), NOT the monotonic LRU ordinal
    /// (that orders eviction only).
    fn is_expired(ttl_ms: u64, updated_ms: u64, now: u64) -> bool {
        ttl_ms != 0 && now.saturating_sub(updated_ms) > ttl_ms
    }

    /// The current snapshot for `request_id`, returning events with `seq > since`.
    /// `None` when the build is unknown (never tracked or already swept), OR when
    /// it is EXPIRED past the TTL — checked on the READ so a quiet process (no
    /// intervening `emit` to run the sweep) still returns `not_found` for an
    /// expired id, and the stale track is lazily evicted here. An unknown/expired
    /// id is `not_found`, not an error.
    pub fn snapshot(&self, request_id: &str, since: u64) -> Option<Snapshot> {
        let now = now_ms();
        // Clamp identically to the write path so a bounded key matches itself.
        let request_id = clamp_request_id(request_id);
        let request_id = request_id.as_str();
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        // Read-side expiry: an expired build reads as not_found even with no emit.
        let expired = match map.get(request_id) {
            Some(t) => Self::is_expired(self.ttl_ms, t.updated_ms, now),
            None => return None,
        };
        if expired {
            map.remove(request_id); // lazily evict so it does not linger
            return None;
        }
        let track = map.get(request_id)?;
        let events: Vec<ProgressEvent> = track
            .events
            .iter()
            .filter(|e| e.seq > since)
            .cloned()
            .collect();
        let (step, total) = track
            .events
            .iter()
            .rev()
            .find_map(|e| match (e.step, e.total) {
                (Some(s), Some(t)) => Some((Some(s), Some(t))),
                _ => None,
            })
            .unwrap_or((None, None));
        Some(Snapshot {
            request_id: request_id.to_string(),
            stage: track.snapshot_stage(),
            terminal: track.terminal.is_some(),
            created_ms: track.created_ms,
            updated_ms: track.updated_ms,
            last_seq: track.next_seq.saturating_sub(1),
            step,
            total,
            events,
        })
    }

    /// A live subscription for `request_id`: the current snapshot (events since
    /// `since`) plus a broadcast [`Receiver`](broadcast::Receiver) for events
    /// emitted AFTER this call. Returns `None` for an unknown build. Taking the
    /// receiver under the same lock that reads the snapshot closes the race where
    /// an event could slip between the snapshot read and the subscribe.
    pub fn subscribe(
        &self,
        request_id: &str,
        since: u64,
    ) -> Option<(Snapshot, broadcast::Receiver<ProgressEvent>)> {
        // Clamp identically to the write path so a bounded key matches itself.
        let request_id = clamp_request_id(request_id);
        let request_id = request_id.as_str();
        let rx = {
            let now = now_ms();
            let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            // Read-side expiry applies here too (subscribing to an expired id must
            // not resurrect it): expired → lazily evict + not_found.
            let expired = match map.get(request_id) {
                Some(t) => Self::is_expired(self.ttl_ms, t.updated_ms, now),
                None => return None,
            };
            if expired {
                map.remove(request_id);
                return None;
            }
            map.get(request_id)?.tx.subscribe()
        };
        let snap = self.snapshot(request_id, since)?;
        Some((snap, rx))
    }

    /// Long-poll: return immediately with any events after `since`; otherwise
    /// wait up to `wait` for the next event, then return a fresh snapshot. A
    /// terminal build returns at once (nothing more will come). An unknown build
    /// returns `None`. On subscriber disconnect the receiver simply drops — the
    /// build is unaffected (the tx lives in the track). If the build EXPIRES past
    /// the TTL mid-wait, the post-wait `snapshot` applies read-side expiry and
    /// resolves to `None` (not_found) rather than returning stale data.
    pub async fn poll(
        &self,
        request_id: &str,
        since: u64,
        wait: std::time::Duration,
    ) -> Option<Snapshot> {
        let (snap, mut rx) = self.subscribe(request_id, since)?;
        if !snap.events.is_empty() || snap.terminal || wait.is_zero() {
            return Some(snap);
        }
        // Nothing new yet — await a live event (or timeout), then re-snapshot so
        // the caller always gets the full current state, not just one delta. The
        // re-snapshot also re-checks TTL, so an id that expired during the wait
        // resolves to not_found instead of hanging or returning stale data.
        let _ = tokio::time::timeout(wait, rx.recv()).await;
        self.snapshot(request_id, since)
    }

    /// Test helper: number of tracked builds.
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// Test helper: backdate a build's wall-clock activity so read-side TTL expiry
    /// can be exercised without sleeping. No-op if the build is unknown.
    #[cfg(test)]
    fn force_updated_ms(&self, request_id: &str, updated_ms: u64) {
        if let Some(t) = self.inner.lock().unwrap().get_mut(request_id) {
            t.updated_ms = updated_ms;
        }
    }
}

/// A per-build log tap handed to the build subprocess runner (BLD-05 `run`).
/// It parses each captured stdout/stderr LINE for cargo build progress
/// (`{step,total}`) and emits a throttled [`Stage::Building`] event — so a GUI
/// shows a real progress bar while the build runs, not just before/after.
/// Cloneable (cheap) so the stdout and stderr drain tasks can share it.
#[derive(Clone)]
pub struct BuildTap {
    request_id: String,
}

impl BuildTap {
    pub fn new(request_id: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
        }
    }

    /// Feed one ALREADY-SANITIZED output line. Parses `{step,total}` and emits a
    /// building event (throttled by the bus). The caller MUST redact the line
    /// (S6/S7) before calling — the bus never sees a raw secret.
    pub fn on_line(&self, sanitized_line: &str) {
        if let Some((step, total)) = parse_cargo_progress(sanitized_line) {
            bus().emit(
                &self.request_id,
                Emit::stage(Stage::Building).progress(step, total),
            );
        }
    }
}

/// Parse a cargo build progress signal `{step,total}` from one output line.
///
/// Cargo renders human progress on stderr as `Building [==>  ] 12/34: crate …`
/// (or `Compiling`); with `--message-format=json` each `compiler-artifact`
/// message advances the count. We recognize the `N/M` form, which appears in the
/// human progress bar and is the reliable step/total signal (the JSON stream has
/// no up-front total). Returns `None` for any line without a plausible `N/M`
/// where `N <= M` and `M > 0`.
pub fn parse_cargo_progress(line: &str) -> Option<(u32, u32)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'/' {
                let num = &line[start..i];
                i += 1;
                let dstart = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i > dstart {
                    let den = &line[dstart..i];
                    if let (Ok(n), Ok(m)) = (num.parse::<u32>(), den.parse::<u32>()) {
                        if m > 0 && n <= m {
                            return Some((n, m));
                        }
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cargo_progress_bar() {
        assert_eq!(
            parse_cargo_progress("   Building [=======>       ] 12/34: terminus"),
            Some((12, 34))
        );
        assert_eq!(
            parse_cargo_progress("    Compiling 3/3: serde_json"),
            Some((3, 3))
        );
    }

    #[test]
    fn rejects_non_progress_lines() {
        assert_eq!(parse_cargo_progress("Finished release [optimized]"), None);
        assert_eq!(parse_cargo_progress("no numbers here"), None);
        // m == 0 is not a valid total.
        assert_eq!(parse_cargo_progress("m == 0: 0/0"), None);
        // n > m: the first plausible pair must satisfy n <= m; here it doesn't
        // and there is no other pair, so None.
        assert_eq!(parse_cargo_progress("ratio 5/3 only"), None);
    }

    #[test]
    fn emit_orders_and_returns_current_plus_recent() {
        let bus = ProgressBus::with_bounds(64, 8, 0);
        let id = "req-1";
        bus.emit(id, Emit::stage(Stage::Queued));
        bus.emit(id, Emit::stage(Stage::Scheduled));
        bus.emit(id, Emit::stage(Stage::Building).progress(1, 10));
        let snap = bus.snapshot(id, 0).expect("tracked");
        assert_eq!(snap.stage, Stage::Building);
        let seqs: Vec<u64> = snap.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        assert_eq!(snap.step, Some(1));
        assert_eq!(snap.total, Some(10));
        // A late subscriber (since=last_seq) gets no deltas but still the state.
        let late = bus.snapshot(id, snap.last_seq).unwrap();
        assert!(late.events.is_empty());
        assert_eq!(late.stage, Stage::Building);
        assert_eq!(late.step, Some(1));
    }

    #[test]
    fn since_cursor_returns_only_new_events() {
        let bus = ProgressBus::with_bounds(64, 8, 0);
        let id = "req-cursor";
        bus.emit(id, Emit::stage(Stage::Queued));
        bus.emit(id, Emit::stage(Stage::Scheduled));
        let after = bus.snapshot(id, 1).unwrap();
        assert_eq!(after.events.len(), 1);
        assert_eq!(after.events[0].stage, Stage::Scheduled);
    }

    #[test]
    fn building_step_is_throttled_but_advances() {
        let bus = ProgressBus::with_bounds(64, 8, 0);
        let id = "req-throttle";
        bus.emit(id, Emit::stage(Stage::Building).progress(1, 10));
        bus.emit(id, Emit::stage(Stage::Building).progress(1, 10)); // dup → dropped
        bus.emit(id, Emit::stage(Stage::Building).progress(2, 10)); // advances
        let snap = bus.snapshot(id, 0).unwrap();
        let building: Vec<_> = snap
            .events
            .iter()
            .filter(|e| e.stage == Stage::Building)
            .collect();
        assert_eq!(building.len(), 2, "duplicate step throttled");
        assert_eq!(snap.step, Some(2));
    }

    #[test]
    fn building_started_transition_is_always_retained_without_progress() {
        // A build whose cargo output has NO parseable N/M line: the tap never
        // emits a `{step,total}` tick, but the explicit "build started" transition
        // MUST still be retained, so the stream shows the full stage sequence.
        let bus = ProgressBus::with_bounds(64, 8, 0);
        let id = "req-no-progress";
        bus.emit(id, Emit::stage(Stage::Queued));
        bus.emit(id, Emit::stage(Stage::Scheduled));
        bus.emit(id, Emit::stage(Stage::Relaying)); // remote-style; still valid
        bus.emit(id, Emit::stage(Stage::Building)); // STARTED, no {step,total}
        bus.emit(id, Emit::stage(Stage::Publishing));
        bus.emit(id, Emit::stage(Stage::Published).sha("f00d"));
        let snap = bus.snapshot(id, 0).unwrap();
        let stages: Vec<&str> = snap.events.iter().map(|e| e.stage.as_str()).collect();
        assert_eq!(
            stages,
            vec![
                "queued",
                "scheduled",
                "relaying",
                "building",
                "publishing",
                "published"
            ],
            "every stage transition retained even with no progress ticks"
        );
        // The building-started event carries no {step,total} (it's a transition).
        let b = snap
            .events
            .iter()
            .find(|e| e.stage == Stage::Building)
            .unwrap();
        assert_eq!((b.step, b.total), (None, None));
    }

    #[test]
    fn relaying_is_remote_only_local_stream_valid_without_it() {
        // Contract: `relaying` appears only on the REMOTE/heavy path (rsync to the
        // build host). A LOCAL build's stream is valid WITHOUT it.
        let bus = ProgressBus::with_bounds(64, 8, 0);

        // LOCAL: scheduled → building directly, NO relaying.
        let local = "req-local";
        for st in [
            Stage::Queued,
            Stage::Scheduled,
            Stage::Building,
            Stage::Publishing,
            Stage::Published,
        ] {
            bus.emit(local, Emit::stage(st));
        }
        let ls = bus.snapshot(local, 0).unwrap();
        assert!(
            !ls.events.iter().any(|e| e.stage == Stage::Relaying),
            "local build must not emit relaying"
        );
        assert!(ls.terminal && ls.stage == Stage::Published);

        // REMOTE: includes relaying between scheduled and building.
        let remote = "req-remote";
        for st in [
            Stage::Queued,
            Stage::Scheduled,
            Stage::Relaying,
            Stage::Building,
            Stage::Publishing,
            Stage::Published,
        ] {
            bus.emit(remote, Emit::stage(st));
        }
        let rs = bus.snapshot(remote, 0).unwrap();
        let idx = |st: Stage| rs.events.iter().position(|e| e.stage == st).unwrap();
        assert!(
            idx(Stage::Scheduled) < idx(Stage::Relaying)
                && idx(Stage::Relaying) < idx(Stage::Building),
            "remote build emits relaying between scheduled and building"
        );
    }

    #[test]
    fn ring_buffer_bounds_retention() {
        let bus = ProgressBus::with_bounds(4, 8, 0);
        let id = "req-ring";
        for i in 0..10 {
            bus.emit(id, Emit::stage(Stage::Building).progress(i, 100));
        }
        let snap = bus.snapshot(id, 0).unwrap();
        assert_eq!(snap.events.len(), 4, "ring buffer capped at max_events");
        // Oldest fell off; last_seq still reflects total emitted.
        assert_eq!(snap.last_seq, 10);
        assert_eq!(snap.events.first().unwrap().seq, 7);
    }

    #[test]
    fn evicts_least_recently_updated_when_full() {
        // DETERMINISTIC regardless of wall-clock resolution or test parallelism:
        // LRU order is the monotonic touch ordinal, so even though a/b/c are
        // emitted within the same clock tick their order is strict (a<b<c) and
        // eviction can never tie.
        let bus = ProgressBus::with_bounds(16, 2, 0);
        bus.emit("a", Emit::stage(Stage::Queued)); // touch 0 → LRU
        bus.emit("b", Emit::stage(Stage::Queued)); // touch 1
                                                   // At capacity (2); a 3rd distinct build evicts the strict LRU ("a").
        bus.emit("c", Emit::stage(Stage::Queued)); // touch 2
        assert_eq!(bus.tracked(), 2);
        assert!(bus.snapshot("a", 0).is_none(), "strict LRU build evicted");
        assert!(bus.snapshot("b", 0).is_some());
        assert!(bus.snapshot("c", 0).is_some());
        // Touching "b" makes it most-recently-used, so the next distinct build
        // evicts "c" (now the strict LRU) — proving order tracks activity.
        bus.emit("b", Emit::stage(Stage::Scheduled)); // touch 3 → b now MRU
        bus.emit("d", Emit::stage(Stage::Queued)); // touch 4 → evicts LRU "c"
        assert_eq!(bus.tracked(), 2);
        assert!(
            bus.snapshot("c", 0).is_none(),
            "c became LRU and was evicted"
        );
        assert!(bus.snapshot("b", 0).is_some(), "b was refreshed, retained");
        assert!(bus.snapshot("d", 0).is_some());
    }

    #[test]
    fn ttl_sweeps_idle_builds() {
        // ttl of 1ms; emit for "old", sleep past it, then any write sweeps "old".
        let bus = ProgressBus::with_bounds(16, 8, 1);
        bus.emit("old", Emit::stage(Stage::Queued));
        std::thread::sleep(std::time::Duration::from_millis(5));
        bus.emit("new", Emit::stage(Stage::Queued));
        assert!(bus.snapshot("old", 0).is_none(), "idle build swept by ttl");
        assert!(bus.snapshot("new", 0).is_some());
    }

    #[test]
    fn unknown_build_is_none_not_error() {
        let bus = ProgressBus::with_bounds(16, 8, 0);
        assert!(bus.snapshot("never", 0).is_none());
    }

    #[tokio::test]
    async fn read_side_ttl_expiry_returns_not_found_without_emit() {
        // A QUIET process: no emit runs the sweep, so expiry must be enforced on
        // the READ. ttl = 1000ms; backdate "stale" well past it, leave "live"
        // fresh. Neither snapshot nor poll receives an intervening emit.
        let bus = ProgressBus::with_bounds(16, 8, 1000);
        bus.emit("live", Emit::stage(Stage::Queued));
        bus.emit("stale", Emit::stage(Stage::Queued));
        bus.force_updated_ms("stale", now_ms().saturating_sub(5_000));

        // snapshot: expired → not_found (None); within-TTL build unaffected.
        assert!(
            bus.snapshot("stale", 0).is_none(),
            "expired build reads as not_found via snapshot with no emit"
        );
        assert!(
            bus.snapshot("live", 0).is_some(),
            "within-TTL build unaffected"
        );
        // The stale track was lazily evicted on the expired read.
        assert!(bus.snapshot("stale", 0).is_none());

        // poll: same contract — expired → None, live → Some.
        assert!(bus
            .poll("stale", 0, std::time::Duration::from_millis(0))
            .await
            .is_none());
        assert!(bus
            .poll("live", 0, std::time::Duration::from_millis(0))
            .await
            .is_some());
    }

    #[tokio::test]
    async fn poll_resolves_not_found_if_build_expires_mid_wait() {
        // ttl = 1ms; a caught-up poller (since = last_seq) with no incoming emit
        // waits 60ms → the build expires DURING the wait → the post-wait snapshot
        // applies read-side expiry and resolves to not_found (not stale data).
        let bus = ProgressBus::with_bounds(16, 8, 1);
        bus.emit("q", Emit::stage(Stage::Scheduled)); // non-terminal, last_seq = 1
        let snap = bus.poll("q", 1, std::time::Duration::from_millis(60)).await;
        assert!(snap.is_none(), "id expired mid-wait resolves to not_found");
    }

    #[test]
    fn terminal_states_carry_sha_or_error() {
        let bus = ProgressBus::with_bounds(16, 8, 0);
        bus.emit("ok", Emit::stage(Stage::Publishing));
        bus.emit("ok", Emit::stage(Stage::Published).sha("abc123"));
        let s = bus.snapshot("ok", 0).unwrap();
        assert!(s.terminal);
        assert_eq!(s.stage, Stage::Published);
        assert_eq!(s.events.last().unwrap().sha.as_deref(), Some("abc123"));

        bus.emit("bad", Emit::stage(Stage::Building).progress(1, 2));
        bus.emit(
            "bad",
            Emit::stage(Stage::Failed).message("error: could not compile foo"),
        );
        let f = bus.snapshot("bad", 0).unwrap();
        assert!(f.terminal);
        assert_eq!(f.stage, Stage::Failed);
        assert_eq!(
            f.events.last().unwrap().message.as_deref(),
            Some("error: could not compile foo")
        );

        // Downstream lifecycle stages are valid too.
        bus.emit("dep", Emit::stage(Stage::Deployed).sha("deadbeef"));
        assert_eq!(bus.snapshot("dep", 0).unwrap().stage, Stage::Deployed);
        bus.emit(
            "rb",
            Emit::stage(Stage::RolledBack).message("health gate failed"),
        );
        assert!(bus.snapshot("rb", 0).unwrap().terminal);
    }

    #[test]
    fn terminal_stream_ignores_later_events() {
        // Terminal integrity: once a stream is terminal, later events are ignored,
        // so a terminal snapshot can never report an inconsistent later stage.
        let bus = ProgressBus::with_bounds(64, 8, 0);
        let id = "req-terminal";
        bus.emit(id, Emit::stage(Stage::Building).progress(1, 3));
        bus.emit(id, Emit::stage(Stage::Published).sha("abc"));
        // A late (buggy/racing) building event AFTER the terminal is dropped.
        bus.emit(id, Emit::stage(Stage::Building).progress(2, 3));
        bus.emit(id, Emit::stage(Stage::Failed).message("too late"));
        let snap = bus.snapshot(id, 0).unwrap();
        assert!(snap.terminal);
        assert_eq!(snap.stage, Stage::Published, "stays at the terminal stage");
        // No event past the terminal `published` was retained.
        assert_eq!(snap.events.last().unwrap().stage, Stage::Published);
        assert!(!snap
            .events
            .iter()
            .any(|e| e.stage == Stage::Building && e.step == Some(2)));
    }

    #[test]
    fn huge_message_is_truncated_to_the_bound() {
        let bus = ProgressBus::with_bounds(64, 8, 0);
        let id = "req-hugemsg";
        let huge = "x".repeat(MAX_MESSAGE_LEN * 4);
        bus.emit(id, Emit::stage(Stage::Failed).message(huge));
        let snap = bus.snapshot(id, 0).unwrap();
        let msg = snap.events.last().unwrap().message.as_ref().unwrap();
        // Bounded: at most the cap plus the marker, and it notes the truncation.
        assert!(msg.len() <= MAX_MESSAGE_LEN + "…[truncated]".len());
        assert!(msg.ends_with("…[truncated]"));
    }

    #[test]
    fn oversized_request_id_is_bounded_and_matches_on_read() {
        let bus = ProgressBus::with_bounds(64, 8, 0);
        let long = "z".repeat(MAX_REQUEST_ID_LEN * 3);
        bus.emit(&long, Emit::stage(Stage::Queued));
        // Stored id is clamped, and a read with the SAME long id still finds it
        // (read path clamps identically).
        let snap = bus.snapshot(&long, 0).unwrap();
        assert!(snap.request_id.len() <= MAX_REQUEST_ID_LEN);
        assert_eq!(
            snap.events.last().unwrap().request_id.len(),
            snap.request_id.len()
        );
    }

    #[test]
    fn build_tap_emits_building_from_sanitized_line() {
        // The tap targets the GLOBAL bus; a unique id avoids cross-test collision.
        let id = format!("tap-{}", uuid::Uuid::new_v4());
        let tap = BuildTap::new(&id);
        tap.on_line("   Building [====>    ] 7/20: serde");
        tap.on_line("this line has no progress");
        let snap = bus().snapshot(&id, 0).expect("tap created the track");
        assert_eq!(snap.stage, Stage::Building);
        assert_eq!(snap.step, Some(7));
        assert_eq!(snap.total, Some(20));
    }

    #[tokio::test]
    async fn poll_returns_immediately_on_pending_events() {
        let bus = ProgressBus::with_bounds(16, 8, 0);
        bus.emit("p", Emit::stage(Stage::Queued));
        // since=0 → there is already an event → returns at once (no wait).
        let snap = bus
            .poll("p", 0, std::time::Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(snap.events.len(), 1);
    }

    #[tokio::test]
    async fn poll_wakes_on_live_event() {
        // A caught-up poller blocks, then a concurrent emit wakes it.
        let bus = std::sync::Arc::new(ProgressBus::with_bounds(16, 8, 0));
        bus.emit("live", Emit::stage(Stage::Queued));
        let b2 = bus.clone();
        let waiter =
            tokio::spawn(
                async move { b2.poll("live", 1, std::time::Duration::from_secs(5)).await },
            );
        // Give the waiter a moment to subscribe, then emit.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        bus.emit("live", Emit::stage(Stage::Scheduled));
        let snap = waiter.await.unwrap().unwrap();
        assert!(snap.events.iter().any(|e| e.stage == Stage::Scheduled));
    }

    #[tokio::test]
    async fn poll_zero_wait_returns_current_snapshot() {
        let bus = ProgressBus::with_bounds(16, 8, 0);
        bus.emit("z", Emit::stage(Stage::Queued));
        let snap = bus
            .poll("z", 1, std::time::Duration::from_millis(0))
            .await
            .unwrap();
        assert!(snap.events.is_empty());
        assert_eq!(snap.stage, Stage::Queued);
    }

    #[tokio::test]
    async fn poll_unknown_build_is_none() {
        let bus = ProgressBus::with_bounds(16, 8, 0);
        assert!(bus
            .poll("nope", 0, std::time::Duration::from_millis(0))
            .await
            .is_none());
    }
}
