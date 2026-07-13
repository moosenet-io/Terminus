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
//! during the build). This stream covers `compiler_build`'s OWN scope only —
//! stages, in order:
//!   `queued → scheduled → [relaying (remote only)] → building{step,total} →
//!    publishing → published | failed`
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
//! ## Terminal stages (and what is NOT on this stream)
//! `compiler_build`'s scope ENDS at publish, so this stream has exactly two
//! terminal stages: [`Stage::Published`] (with the artifact sha) on success and
//! [`Stage::Failed`] (with a sanitized error tail) on failure. Once terminal, the
//! stream is CLOSED — later events are ignored. The downstream updater/deploy
//! stage (BLD-13) — `deployed` / `rolled_back` — is a SEPARATE lifecycle that is
//! NOT emitted onto this stream (there is no `Deployed`/`RolledBack` variant), so
//! the code and this doc agree: nothing follows `published`/`failed` here.
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
/// Max length of a `request_id`. This is a HARD VALIDATION bound enforced at the
/// tool boundary (`compiler_build`/`compiler_progress`), NOT a lossy clamp — an
/// overlong id is REJECTED, never truncated, so two distinct ids can never be
/// silently folded onto one track. Bounds memory (the id is a map key + echoed in
/// every event) without any collision risk.
pub const MAX_REQUEST_ID_LEN: usize = 128;

/// Whether `id` is within the bounded length. The tool boundary rejects a longer
/// id with a clear validation error rather than clamping it.
pub fn request_id_len_ok(id: &str) -> bool {
    id.len() <= MAX_REQUEST_ID_LEN
}

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

/// Run `f` at the progress-bus boundary FAIL-OPEN: an unexpected panic inside bus
/// logic is caught + logged and MUST NOT propagate, so a bus hiccup can never
/// abort the build that is only reporting progress. (The bus is panic-free by
/// construction — this is defense in depth.) Returns `None` if `f` panicked.
fn fail_open<T>(what: &str, f: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::error!("compiler progress bus: {what} panicked (swallowed, fail-open)");
            None
        }
    }
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

/// A build lifecycle stage on the `compiler_build` progress stream. Ordered as
/// emitted; the two terminal stages ([`is_terminal`](Stage::is_terminal)) close
/// the stream.
///
/// This stream covers `compiler_build`'s OWN scope only — `queued → scheduled →
/// [relaying] → building → publishing → published | failed`. The downstream
/// updater/deploy stage (BLD-13) — `deployed` / `rolled_back` — is a SEPARATE
/// lifecycle NOT emitted onto this stream (the stream is closed at `published`/
/// `failed`), so there is no `Deployed`/`RolledBack` variant here.
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
        }
    }

    /// Whether this stage closes the stream (no further events expected). Only
    /// `published`/`failed` are terminal for this bus; `deployed`/`rolled_back`
    /// are the downstream updater's concern and never reach this stream.
    pub fn is_terminal(self) -> bool {
        matches!(self, Stage::Published | Stage::Failed)
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
    /// On [`Stage::Published`]: the artifact sha256.
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
    /// A globally-unique EPOCH assigned when THIS track is created. If the track
    /// is evicted (LRU/TTL) and a NEW build reuses the same `request_id`, the new
    /// track gets a fresh generation. A long-poller captures the generation at
    /// subscribe and, after waking, only returns a snapshot whose generation still
    /// matches — otherwise the id was reused mid-wait and it resolves to not_found
    /// (never the WRONG build's data). The API is per-build-request, not per-slot.
    generation: u64,
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
    fn new(now: u64, touched: u64, generation: u64, max_events: usize) -> Self {
        let depth = BROADCAST_DEPTH.max(max_events.min(1024));
        let (tx, _rx) = broadcast::channel(depth);
        Self {
            events: Vec::new(),
            next_seq: 1,
            created_ms: now,
            updated_ms: now,
            generation,
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
    /// Monotonic generation counter: every NEW track gets the next value, so a
    /// reused `request_id` (old track evicted, new build under the same id) is
    /// distinguishable by a long-poller (see `Track::generation`).
    generation: AtomicU64,
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
    /// The track generation this snapshot was read from (see `Track::generation`).
    pub generation: u64,
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
            "generation": self.generation,
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
            generation: AtomicU64::new(0),
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
            generation: AtomicU64::new(0),
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

    /// BEGIN a fresh progress stream for `request_id`, ROTATING the track to a new
    /// generation: any existing track for this id (still-live OR already-terminal)
    /// is REPLACED by a brand-new one — fresh generation, empty event ring,
    /// non-terminal. A build calls this before its first `queued` emit, so reusing
    /// a request_id that is still tracked always yields a CLEAN per-build stream:
    /// no stale terminal state is shown, and the new build's events are never
    /// dropped by the old track's terminal-close.
    ///
    /// Race-safe with concurrent readers: the rotation replaces the map entry under
    /// the lock, which DROPS the old track (and its broadcast sender), so a reader
    /// mid-poll on the OLD generation wakes (channel closed) and resolves via the
    /// generation check to not_found — it never mixes the two builds' data.
    /// Returns the new generation. Panic-safe (fail-open).
    pub fn begin(&self, request_id: &str) -> u64 {
        fail_open("begin", || self.begin_inner(request_id)).unwrap_or(0)
    }

    fn begin_inner(&self, request_id: &str) -> u64 {
        let now = now_ms();
        let touched = self.touch.fetch_add(1, Ordering::Relaxed);
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        let max_events = self.max_events;
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        Self::sweep_locked(&mut map, self.ttl_ms, now);
        // Make room only if this is a genuinely NEW id (rotation replaces in place).
        Self::evict_if_full_locked(&mut map, self.max_builds, request_id);
        // `insert` REPLACES any existing track → the old one (and its tx) is
        // dropped, closing old subscribers' channels.
        map.insert(
            request_id.to_string(),
            Track::new(now, touched, generation, max_events),
        );
        generation
    }

    /// Emit an event for `request_id`, creating the track if new. Returns the
    /// assigned seq. PANIC-SAFE (fail-open): any unexpected panic inside the bus is
    /// caught + logged and never propagates, so the bus can't abort the build that
    /// is only reporting progress. Applies TTL sweep + capacity eviction +
    /// ring-buffer bound, and (for the building stage) throttles unchanged
    /// `{step,total}`.
    pub fn emit(&self, request_id: &str, e: Emit) -> u64 {
        fail_open("emit", || self.emit_inner(request_id, e)).unwrap_or(0)
    }

    fn emit_inner(&self, request_id: &str, e: Emit) -> u64 {
        let now = now_ms();
        // The id length is a HARD validation rule enforced at the tool boundary
        // (`compiler_build`/`compiler_progress`), so an id reaching the bus is
        // already bounded and is used VERBATIM — never clamped (a lossy clamp
        // could fold two distinct ids onto one track).
        let rid = request_id;
        // A strictly-increasing LRU ordinal for THIS emit — assigned to the
        // touched build below so eviction order never depends on wall-clock
        // resolution (two emits in the same tick still order strictly).
        let touched = self.touch.fetch_add(1, Ordering::Relaxed);
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());

        Self::sweep_locked(&mut map, self.ttl_ms, now);
        Self::evict_if_full_locked(&mut map, self.max_builds, rid);

        let max_events = self.max_events;
        // A fresh generation for a NEW track (so a reused id is distinguishable).
        let new_gen = self.generation.fetch_add(1, Ordering::Relaxed);
        let track = map
            .entry(rid.to_string())
            .or_insert_with(|| Track::new(now, touched, new_gen, max_events));

        // TERMINAL INTEGRITY: once a stream is terminal (published/failed) it is
        // CLOSED — a later event is ignored, so a terminal snapshot can never
        // report an inconsistent later stage (e.g. building after published).
        // Deterministic close.
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
            request_id: rid.to_string(),
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

    /// Build a [`Snapshot`] from a `&Track` under the caller's lock (no re-lock).
    /// The generation is read from THIS track, so a snapshot's generation always
    /// belongs to the exact track it was read from — the invariant the atomic
    /// [`subscribe`](Self::subscribe) relies on.
    fn build_snapshot(request_id: &str, since: u64, track: &Track) -> Snapshot {
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
        Snapshot {
            request_id: request_id.to_string(),
            generation: track.generation,
            stage: track.snapshot_stage(),
            terminal: track.terminal.is_some(),
            created_ms: track.created_ms,
            updated_ms: track.updated_ms,
            last_seq: track.next_seq.saturating_sub(1),
            step,
            total,
            events,
        }
    }

    /// The current snapshot for `request_id`, returning events with `seq > since`.
    /// `None` when the build is unknown (never tracked or already swept), OR when
    /// it is EXPIRED past the TTL — checked on the READ so a quiet process (no
    /// intervening `emit` to run the sweep) still returns `not_found` for an
    /// expired id, and the stale track is lazily evicted here. An unknown/expired
    /// id is `not_found`, not an error.
    pub fn snapshot(&self, request_id: &str, since: u64) -> Option<Snapshot> {
        let now = now_ms();
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
        Some(Self::build_snapshot(request_id, since, track))
    }

    /// A live subscription for `request_id`: the current snapshot (events since
    /// `since`) plus a broadcast [`Receiver`](broadcast::Receiver) for events
    /// emitted AFTER this call — captured ATOMICALLY, under ONE lock hold on the
    /// SAME [`Track`], together with that track's generation (carried in the
    /// snapshot). There is NO window between "which track did I subscribe to" and
    /// "which track's snapshot/generation did I capture": the receiver, the
    /// snapshot, and the generation all come from the same track instance, so a
    /// concurrent evict+reuse of the id can never pair an OLD receiver with a
    /// NEW-generation snapshot. Returns `None` for an unknown/expired build.
    pub fn subscribe(
        &self,
        request_id: &str,
        since: u64,
    ) -> Option<(Snapshot, broadcast::Receiver<ProgressEvent>)> {
        let now = now_ms();
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        // Read-side expiry applies here too (subscribing to an expired id must not
        // resurrect it): expired → lazily evict + not_found.
        let expired = match map.get(request_id) {
            Some(t) => Self::is_expired(self.ttl_ms, t.updated_ms, now),
            None => return None,
        };
        if expired {
            map.remove(request_id);
            return None;
        }
        let track = map.get(request_id)?;
        // rx + snapshot + generation ALL from the same `track`, under one lock.
        let rx = track.tx.subscribe();
        let snap = Self::build_snapshot(request_id, since, track);
        Some((snap, rx))
    }

    /// Long-poll: return immediately with any events after `since`; otherwise
    /// wait up to `wait` for the next event, then return a fresh snapshot. A
    /// terminal build returns at once (nothing more will come). An unknown build
    /// returns `None`. On subscriber disconnect the receiver simply drops — the
    /// build is unaffected (the tx lives in the track). If the build EXPIRES past
    /// the TTL mid-wait, the post-wait `snapshot` applies read-side expiry and
    /// resolves to `None` (not_found) rather than returning stale data.
    ///
    /// GENERATION SAFETY: `subscribe` captures the receiver AND the generation from
    /// the SAME track atomically, so `gen0` below is the generation of the exact
    /// track this waiter subscribed to. If that track is EVICTED/ROTATED and the id
    /// is REUSED by a NEW build mid-wait, the post-wait snapshot has a different
    /// generation and resolves to `None` (not_found) — the old waiter never
    /// receives the new build's data (tracks are per-build-request, not per-slot).
    pub async fn poll(
        &self,
        request_id: &str,
        since: u64,
        wait: std::time::Duration,
    ) -> Option<Snapshot> {
        let (snap, mut rx) = self.subscribe(request_id, since)?;
        // `gen0` is the SUBSCRIBED track's generation (from the atomic subscribe),
        // NOT a separately-taken snapshot — no cross-generation TOCTOU.
        let gen0 = snap.generation;
        if !snap.events.is_empty() || snap.terminal || wait.is_zero() {
            return Some(snap);
        }
        // Nothing new yet — await a live event (or timeout), then re-snapshot so
        // the caller always gets the full current state, not just one delta. The
        // re-snapshot also re-checks TTL, so an id that expired during the wait
        // resolves to not_found instead of hanging or returning stale data.
        let _ = tokio::time::timeout(wait, rx.recv()).await;
        let after = self.snapshot(request_id, since)?;
        // If the id was reused by a NEW build mid-wait, the generation differs →
        // do NOT return the new build's data to this (stale) waiter.
        if after.generation != gen0 {
            return None;
        }
        Some(after)
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
        // `published` and `failed` are the ONLY terminal stages on this stream;
        // deployed/rolled_back are the downstream updater's concern (no variant).
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
    fn distinct_ids_sharing_a_prefix_never_collide() {
        // The bus stores ids VERBATIM (no lossy clamp), so two distinct ids that
        // share a long common prefix are separate tracks — never folded into one.
        // (The length bound is a hard validation rule at the TOOL boundary.)
        let bus = ProgressBus::with_bounds(64, 8, 0);
        let base = "p".repeat(MAX_REQUEST_ID_LEN);
        let a = format!("{base}A");
        let b = format!("{base}B");
        bus.emit(&a, Emit::stage(Stage::Queued));
        bus.emit(&b, Emit::stage(Stage::Failed).message("b failed"));
        let sa = bus.snapshot(&a, 0).unwrap();
        let sb = bus.snapshot(&b, 0).unwrap();
        assert_eq!(sa.request_id, a);
        assert_eq!(sb.request_id, b);
        assert!(!sa.terminal, "a is its own track (queued)");
        assert!(sb.terminal, "b is its own track (failed)");
        assert_eq!(bus.tracked(), 2, "two distinct ids, two tracks");
    }

    #[test]
    fn request_id_len_ok_bounds_at_the_boundary() {
        assert!(request_id_len_ok(&"x".repeat(MAX_REQUEST_ID_LEN)));
        assert!(!request_id_len_ok(&"x".repeat(MAX_REQUEST_ID_LEN + 1)));
    }

    #[test]
    fn emit_is_fail_open_on_a_panicking_boundary() {
        // The emit boundary must SWALLOW a panic (fail-open) rather than propagate
        // it into the build. `fail_open` is the wrapper `emit` uses.
        let n = fail_open("test-panic", || -> u64 { panic!("boom") });
        assert_eq!(n, None, "a panicking bus op is swallowed, not propagated");
        let ok = fail_open("test-ok", || 42u64);
        assert_eq!(ok, Some(42));
        // And a normal emit path returns without panicking.
        let bus = ProgressBus::with_bounds(8, 4, 0);
        let seq = bus.emit("fo", Emit::stage(Stage::Queued));
        assert_eq!(seq, 1);
    }

    #[test]
    fn a_reused_id_gets_a_fresh_generation() {
        // cap = 1 forces eviction; a same track keeps its generation, but a NEW
        // track under a reused id gets a distinct (higher) generation.
        let bus = ProgressBus::with_bounds(16, 1, 0);
        bus.emit("reuse", Emit::stage(Stage::Queued));
        let g0 = bus.snapshot("reuse", 0).unwrap().generation;
        bus.emit("other", Emit::stage(Stage::Queued)); // evicts "reuse"
        assert!(bus.snapshot("reuse", 0).is_none(), "old track evicted");
        bus.emit("reuse", Emit::stage(Stage::Queued)); // NEW track, same id
        let g1 = bus.snapshot("reuse", 0).unwrap().generation;
        assert!(g1 > g0, "the reused id got a fresh, higher generation");
    }

    #[tokio::test]
    async fn poll_returns_not_found_if_id_reused_by_new_build_midwait() {
        // A waiter blocked on an id's OLD track must NOT receive a DIFFERENT build's
        // data if the id is evicted + reused mid-wait — it resolves to not_found.
        let bus = std::sync::Arc::new(ProgressBus::with_bounds(16, 1, 0)); // cap 1
        bus.emit("x", Emit::stage(Stage::Scheduled)); // old track (gen G0), seq=1
        let b2 = bus.clone();
        // Caught up (since = last_seq) so the poller actually blocks on the old rx.
        let waiter =
            tokio::spawn(async move { b2.poll("x", 1, std::time::Duration::from_secs(5)).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // Evict the old "x" (cap 1), then reuse the id for a brand-new build.
        bus.emit("y", Emit::stage(Stage::Queued)); // evicts old "x"
        bus.emit("x", Emit::stage(Stage::Queued)); // NEW "x" track (gen G1)
        let res = waiter.await.unwrap();
        assert!(
            res.is_none(),
            "the stale waiter resolves not_found, never the new build's data"
        );
    }

    #[test]
    fn subscribe_captures_snapshot_and_generation_from_the_same_track() {
        // Fix 1: subscribe returns the snapshot (with generation) atomically with
        // the receiver, from the SAME track. Rotating the id afterwards must NOT
        // retroactively change the already-captured snapshot's generation.
        let bus = ProgressBus::with_bounds(16, 8, 0);
        bus.emit("id", Emit::stage(Stage::Scheduled));
        let (snap0, _rx0) = bus.subscribe("id", 0).unwrap();
        let gen0 = snap0.generation;
        // A NEW build reuses the id → rotate to a fresh generation.
        let gen1 = bus.begin("id");
        assert_ne!(gen0, gen1, "begin rotates to a new generation");
        // The captured subscription snapshot keeps the OLD generation …
        assert_eq!(snap0.generation, gen0);
        // … and a fresh snapshot now reflects the NEW generation.
        assert_eq!(bus.snapshot("id", 0).unwrap().generation, gen1);
    }

    #[tokio::test]
    async fn poll_returns_not_found_when_id_rotated_by_new_build_midwait() {
        // Fix 1/2: a waiter blocked on the OLD track must resolve to not_found when
        // the id is ROTATED (begin) by a new build mid-wait — never the new data.
        let bus = std::sync::Arc::new(ProgressBus::with_bounds(16, 8, 0));
        bus.emit("x", Emit::stage(Stage::Scheduled)); // old track, non-terminal, seq=1
        let b2 = bus.clone();
        let waiter =
            tokio::spawn(async move { b2.poll("x", 1, std::time::Duration::from_secs(5)).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        bus.begin("x"); // NEW build reuses the id → rotate (drops old tx → wakes waiter)
        bus.emit("x", Emit::stage(Stage::Queued));
        let res = waiter.await.unwrap();
        assert!(
            res.is_none(),
            "the stale waiter resolves not_found after the id was rotated"
        );
    }

    #[test]
    fn begin_rotates_a_terminal_id_to_a_fresh_clean_stream() {
        // Fix 2: reusing an id whose prior build is already TERMINAL must start a
        // clean stream — no stale terminal state, no dropped events.
        let bus = ProgressBus::with_bounds(16, 8, 0);
        // Build A → terminal published.
        bus.emit("id", Emit::stage(Stage::Queued));
        bus.emit("id", Emit::stage(Stage::Published).sha("oldsha"));
        let a = bus.snapshot("id", 0).unwrap();
        assert!(a.terminal && a.stage == Stage::Published);
        let gen_a = a.generation;
        // Build B reuses the id → begin rotates to a fresh stream.
        bus.begin("id");
        bus.emit("id", Emit::stage(Stage::Queued));
        bus.emit("id", Emit::stage(Stage::Building).progress(1, 5));
        let b = bus.snapshot("id", 0).unwrap();
        assert_ne!(b.generation, gen_a, "fresh generation for the reused id");
        assert!(!b.terminal, "the fresh stream is not terminal");
        assert_eq!(b.stage, Stage::Building, "B's own stage, not A's published");
        assert_eq!(
            b.events.first().unwrap().stage,
            Stage::Queued,
            "B's stream starts at queued"
        );
        assert!(
            b.events.iter().all(|e| e.sha.is_none()),
            "no stale published sha from A"
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
