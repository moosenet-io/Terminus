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
//!   `queued → scheduled → relaying → building{step,total} → publishing →
//!    deployed | failed | rolled_back`
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
//!   idle past the TTL is swept on the next write.
//! All three bounds are env-tunable numeric knobs (not infra literals — S1).
//!
//! ## Secret discipline (S6/S7)
//! Log-tail lines are secret-sanitized by the *emitter* (`compiler_build` runs
//! every captured line through its existing redaction set) BEFORE they enter the
//! bus, so a secret never leaves the process through the stream. The bus itself
//! stores only stage/timing/step data and already-redacted text — no secrets, no
//! infra literals.

use std::collections::HashMap;
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
    /// Last emitted `{step,total}`, so building-progress lines that don't advance
    /// the step are dropped (throttle — never a flood of identical events).
    last_step: Option<(u32, u32)>,
    terminal: Option<Stage>,
    tx: broadcast::Sender<ProgressEvent>,
}

impl Track {
    fn new(now: u64, max_events: usize) -> Self {
        let depth = BROADCAST_DEPTH.max(max_events.min(1024));
        let (tx, _rx) = broadcast::channel(depth);
        Self {
            events: Vec::new(),
            next_seq: 1,
            created_ms: now,
            updated_ms: now,
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
    /// we are about to write to). Caller holds the lock.
    fn evict_if_full_locked(map: &mut HashMap<String, Track>, max_builds: usize, keep: &str) {
        while map.len() >= max_builds && !map.contains_key(keep) {
            let victim = map
                .iter()
                .min_by_key(|(_, t)| t.updated_ms)
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
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());

        Self::sweep_locked(&mut map, self.ttl_ms, now);
        Self::evict_if_full_locked(&mut map, self.max_builds, request_id);

        let max_events = self.max_events;
        let track = map
            .entry(request_id.to_string())
            .or_insert_with(|| Track::new(now, max_events));

        // Throttle: a building event that neither advances the step nor carries a
        // message is redundant — drop it (avoid a flood of identical progress).
        if e.stage == Stage::Building && e.message.is_none() {
            match (e.step, e.total) {
                (Some(step), Some(total)) => {
                    if track.last_step == Some((step, total)) {
                        return track.next_seq.saturating_sub(1);
                    }
                    track.last_step = Some((step, total));
                }
                _ => {
                    // A contentless building event carries nothing new — drop it.
                    return track.next_seq.saturating_sub(1);
                }
            }
        } else if e.stage == Stage::Building {
            if let (Some(step), Some(total)) = (e.step, e.total) {
                track.last_step = Some((step, total));
            }
        }

        let seq = track.next_seq;
        track.next_seq += 1;
        track.updated_ms = now;
        if e.stage.is_terminal() {
            track.terminal = Some(e.stage);
        }

        let event = ProgressEvent {
            request_id: request_id.to_string(),
            seq,
            ts_ms: now,
            stage: e.stage,
            step: e.step,
            total: e.total,
            message: e.message,
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

    /// The current snapshot for `request_id`, returning events with `seq > since`.
    /// `None` when the build is unknown (never tracked or already swept) — an
    /// unknown id is `not_found`, not an error.
    pub fn snapshot(&self, request_id: &str, since: u64) -> Option<Snapshot> {
        let map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
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
        let rx = {
            let map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            map.get(request_id)?.tx.subscribe()
        };
        let snap = self.snapshot(request_id, since)?;
        Some((snap, rx))
    }

    /// Long-poll: return immediately with any events after `since`; otherwise
    /// wait up to `wait` for the next event, then return a fresh snapshot. A
    /// terminal build returns at once (nothing more will come). An unknown build
    /// returns `None`. On subscriber disconnect the receiver simply drops — the
    /// build is unaffected (the tx lives in the track).
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
        // the caller always gets the full current state, not just one delta.
        let _ = tokio::time::timeout(wait, rx.recv()).await;
        self.snapshot(request_id, since)
    }

    /// Test helper: number of tracked builds.
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.inner.lock().unwrap().len()
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
        let bus = ProgressBus::with_bounds(16, 2, 0);
        bus.emit("a", Emit::stage(Stage::Queued));
        bus.emit("b", Emit::stage(Stage::Queued));
        // At capacity (2); emitting a 3rd distinct build evicts the LRU ("a").
        bus.emit("c", Emit::stage(Stage::Queued));
        assert_eq!(bus.tracked(), 2);
        assert!(bus.snapshot("a", 0).is_none(), "LRU build evicted");
        assert!(bus.snapshot("c", 0).is_some());
        // An existing build is never evicted by its own emit.
        bus.emit("b", Emit::stage(Stage::Scheduled));
        assert!(bus.snapshot("b", 0).is_some());
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
