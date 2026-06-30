//! CONV-02: In-memory per-user conversation working memory (Tier 1).
//!
//! A `ConversationBuffer` holds recent verbatim turn-pairs (user message +
//! assistant response) for each Matrix user, keyed by user ID. It is the
//! foundation for multi-turn context: CONV-03 prepends the buffer's turns to the
//! Chord request so the model sees recent history.
//!
//! Design (per S78 spec):
//! - Per-user isolation: each user ID maps to its own [`SessionState`].
//! - Session boundary: a session expires after an inactivity timeout
//!   (`LUMINA_SESSION_TIMEOUT_SECS`, default 1800s).
//! - Capacity: bounded by BOTH turn count (`LUMINA_CONV_BUFFER_SIZE`) and a
//!   token budget (`LUMINA_CONV_TOKEN_BUDGET`); FIFO eviction on overflow.
//! - In-memory only: lost on restart by design (Tier 2 / Engram persists).
//! - Token-aware: each entry carries an approximate token count (chars / 4).
//!
//! Time is passed in explicitly as unix seconds (`now_unix`) so session-timeout
//! behaviour is deterministically testable; production callers pass
//! [`unix_now`]. This matches the unix-epoch convention in the parent module.

use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock time as unix seconds. Saturates to 0 before the epoch.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Rough token estimate for a string: ~4 characters per token, rounded up so a
/// short non-empty message still counts as at least one token (otherwise
/// token-budget eviction would never fire for sessions of short messages).
fn approx_tokens(s: &str) -> usize {
    let chars = s.chars().count();
    if chars == 0 { 0 } else { (chars + 3) / 4 }
}

/// One stored conversation turn-pair.
#[derive(Debug, Clone, PartialEq)]
pub struct BufferEntry {
    pub user_message: String,
    pub assistant_response: String,
    /// Unix seconds when this turn-pair was recorded.
    pub timestamp: i64,
    /// Approximate token count for the pair (user + assistant).
    pub approx_tokens: usize,
}

impl BufferEntry {
    fn new(user_message: String, assistant_response: String, timestamp: i64) -> Self {
        let approx_tokens = approx_tokens(&user_message) + approx_tokens(&assistant_response);
        Self { user_message, assistant_response, timestamp, approx_tokens }
    }
}

/// Per-user session state: an ordered window of recent turn-pairs.
#[derive(Debug)]
pub struct SessionState {
    pub entries: VecDeque<BufferEntry>,
    /// CONV-05: compressed summaries of older turns, oldest-first. Always a
    /// logical prefix of the conversation (every summarised turn predates every
    /// verbatim entry), so summaries never interleave with `entries`.
    pub summaries: Vec<SummaryBlock>,
    pub last_activity: i64,
    pub total_tokens: usize,
    /// UUID for correlating this session with an Engram episode (CONV-04).
    pub session_id: String,
}

/// CONV-05: a compact summary that replaces a run of older turn-pairs.
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryBlock {
    pub text: String,
    /// How many turn-pairs this block compresses.
    pub turns_covered: usize,
    pub approx_tokens: usize,
}

impl SessionState {
    fn new(now_unix: i64) -> Self {
        Self {
            entries: VecDeque::new(),
            summaries: Vec::new(),
            last_activity: now_unix,
            total_tokens: 0,
            session_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

/// CONV-05: a unit of work for the async summarizer — the oldest `n` turn-pairs
/// of a specific session to be compressed.
#[derive(Debug, Clone)]
pub struct SummarizationJob {
    pub user_id: String,
    pub session_id: String,
    pub turns: Vec<BufferEntry>,
    pub n: usize,
}

/// Per-user, per-session conversation working memory.
///
/// Wrap in `Arc<RwLock<ConversationBuffer>>` ([`SharedBuffer`]) for concurrent
/// access from async handlers; mutating methods take `&mut self`.
#[derive(Debug)]
pub struct ConversationBuffer {
    max_turns: usize,
    max_tokens: usize,
    session_timeout_secs: i64,
    sessions: HashMap<String, SessionState>,
}

/// Shared handle used across async tasks.
pub type SharedBuffer = std::sync::Arc<std::sync::RwLock<ConversationBuffer>>;

/// Process-global conversation buffer (CONV-03). Initialised once at startup via
/// [`init_global`]; accessed by the agent loop via [`global`]. Mirrors the
/// existing `GLOBAL_EVENT_BUS` pattern so the buffer can persist across messages
/// without threading a handle through every call signature.
static GLOBAL_BUFFER: std::sync::OnceLock<SharedBuffer> = std::sync::OnceLock::new();

/// Install the process-global conversation buffer. Subsequent calls are ignored
/// (the `OnceLock` keeps the first instance). Returns the live handle.
pub fn init_global(buffer: SharedBuffer) -> SharedBuffer {
    let _ = GLOBAL_BUFFER.set(buffer);
    GLOBAL_BUFFER.get().cloned().expect("global buffer set")
}

/// The process-global conversation buffer, if [`init_global`] has run.
pub fn global() -> Option<&'static SharedBuffer> {
    GLOBAL_BUFFER.get()
}

impl ConversationBuffer {
    /// Construct with explicit limits.
    pub fn new(max_turns: usize, max_tokens: usize, session_timeout_secs: i64) -> Self {
        Self {
            // A zero limit would wedge eviction loops; clamp to at least 1 turn.
            max_turns: max_turns.max(1),
            max_tokens: max_tokens.max(1),
            session_timeout_secs: session_timeout_secs.max(0),
            sessions: HashMap::new(),
        }
    }

    /// Construct from environment config (non-secret behavioural vars).
    ///
    /// - `LUMINA_CONV_BUFFER_SIZE`  — max turn-pairs (default [`DEFAULT_BUFFER_SIZE`])
    /// - `LUMINA_CONV_TOKEN_BUDGET` — max tokens in buffer (default [`DEFAULT_TOKEN_BUDGET`])
    /// - `LUMINA_SESSION_TIMEOUT_SECS` — inactivity timeout (default 1800)
    pub fn from_env() -> Self {
        let max_turns = env_usize("LUMINA_CONV_BUFFER_SIZE", DEFAULT_BUFFER_SIZE);
        let max_tokens = env_usize("LUMINA_CONV_TOKEN_BUDGET", DEFAULT_TOKEN_BUDGET);
        let timeout = env_i64("LUMINA_SESSION_TIMEOUT_SECS", DEFAULT_SESSION_TIMEOUT_SECS);
        Self::new(max_turns, max_tokens, timeout)
    }

    /// Wrap a fresh `from_env` buffer in a [`SharedBuffer`].
    pub fn shared_from_env() -> SharedBuffer {
        std::sync::Arc::new(std::sync::RwLock::new(Self::from_env()))
    }

    pub fn max_turns(&self) -> usize { self.max_turns }
    pub fn max_tokens(&self) -> usize { self.max_tokens }
    pub fn session_timeout_secs(&self) -> i64 { self.session_timeout_secs }

    /// Whether `user_id` has an active (non-expired) session as of `now_unix`.
    pub fn is_session_active(&self, user_id: &str, now_unix: i64) -> bool {
        match self.sessions.get(user_id) {
            Some(s) => !self.is_expired(s, now_unix),
            None => false,
        }
    }

    fn is_expired(&self, s: &SessionState, now_unix: i64) -> bool {
        now_unix.saturating_sub(s.last_activity) >= self.session_timeout_secs
    }

    /// Add a turn-pair for `user_id`, starting a new session if none is active.
    ///
    /// Evicts oldest entries until BOTH the turn-count and token budgets hold.
    /// Returns the session id the turn was recorded under.
    pub fn push(
        &mut self,
        user_id: &str,
        user_message: impl Into<String>,
        assistant_response: impl Into<String>,
        now_unix: i64,
    ) -> String {
        // Expired session → start fresh (drops stale turns; CONV-04 flushes them).
        let timeout = self.session_timeout_secs;
        let fresh = match self.sessions.get(user_id) {
            Some(s) => now_unix.saturating_sub(s.last_activity) >= timeout,
            None => true,
        };
        if fresh {
            self.sessions.insert(user_id.to_string(), SessionState::new(now_unix));
        }
        let session = self.sessions.get_mut(user_id).expect("session present");

        let entry = BufferEntry::new(user_message.into(), assistant_response.into(), now_unix);
        session.total_tokens += entry.approx_tokens;
        session.entries.push_back(entry);
        session.last_activity = now_unix;

        // FIFO eviction: respect turn count AND token budget, but always keep the
        // most recent turn even if it alone exceeds the token budget.
        while session.entries.len() > self.max_turns
            || (session.total_tokens > self.max_tokens && session.entries.len() > 1)
        {
            if let Some(old) = session.entries.pop_front() {
                session.total_tokens = session.total_tokens.saturating_sub(old.approx_tokens);
            }
        }
        session.session_id.clone()
    }

    /// Return the active session's turn-pairs (oldest first) for `user_id`.
    /// Empty if there is no session or it has expired.
    pub fn get_context(&self, user_id: &str, now_unix: i64) -> Vec<BufferEntry> {
        match self.sessions.get(user_id) {
            Some(s) if !self.is_expired(s, now_unix) => s.entries.iter().cloned().collect(),
            _ => Vec::new(),
        }
    }

    /// CONV-03: build the prior-turn wire messages (oldest-first, alternating
    /// `user`/`assistant`) for `user_id`'s active session, ready to splice into a
    /// Chord request between the system prompt and the current user message.
    /// Empty when there is no active session (first message → no buffered context).
    pub fn context_messages(&self, user_id: &str, now_unix: i64) -> Vec<super::ChatMessage> {
        let mut out = Vec::new();
        let session = match self.sessions.get(user_id) {
            Some(s) if !self.is_expired(s, now_unix) => s,
            _ => return out,
        };
        // CONV-05: summary blocks (compressed older turns) come first, as system
        // context, then the verbatim recent turns.
        for block in &session.summaries {
            out.push(super::ChatMessage::text(
                "system",
                format!("[Earlier conversation summary] {}", block.text),
            ));
        }
        for e in &session.entries {
            out.push(super::ChatMessage::text("user", e.user_message.as_str()));
            out.push(super::ChatMessage::text("assistant", e.assistant_response.as_str()));
        }
        out
    }

    // ── CONV-05: progressive summarization ──────────────────────────────────

    /// If `user_id`'s active session has at least `threshold` verbatim turns,
    /// return a [`SummarizationJob`] for the oldest half (the turns to compress).
    /// Returns `None` when there's nothing to do (no session, below threshold,
    /// or threshold too small to bother). Pure read — the async summarizer then
    /// computes the summary and calls [`install_summary`].
    pub fn summarization_due(
        &self,
        user_id: &str,
        now_unix: i64,
        threshold: usize,
    ) -> Option<SummarizationJob> {
        let s = self.sessions.get(user_id)?;
        if self.is_expired(s, now_unix) {
            return None;
        }
        let len = s.entries.len();
        if threshold < 2 || len < threshold {
            return None;
        }
        let n = (len / 2).max(1);
        let turns: Vec<BufferEntry> = s.entries.iter().take(n).cloned().collect();
        Some(SummarizationJob {
            user_id: user_id.to_string(),
            session_id: s.session_id.clone(),
            turns,
            n,
        })
    }

    /// Install a computed summary for `job`, replacing the oldest `job.n` verbatim
    /// turns with a [`SummaryBlock`]. No-op (returns `false`) for a stale job — if
    /// the session changed/expired, has fewer than `n` turns now, or its current
    /// front `n` entries no longer match the snapshot `job.turns` (e.g. FIFO
    /// eviction or a racing summarization mutated the front since the snapshot).
    /// This identity fence guarantees the summary text always matches the turns it
    /// replaces. The new block is appended after existing summaries (oldest-first).
    pub fn install_summary(&mut self, job: &SummarizationJob, summary_text: String) -> bool {
        let Some(s) = self.sessions.get_mut(&job.user_id) else { return false };
        if s.session_id != job.session_id || job.n == 0 || s.entries.len() < job.n {
            return false; // stale: session changed or not enough turns
        }
        // Identity fence: the front n entries must still be exactly what we summarized.
        if !s.entries.iter().take(job.n).eq(job.turns.iter()) {
            return false; // front mutated since snapshot — discard this summary
        }
        let mut removed_tokens = 0usize;
        for _ in 0..job.n {
            if let Some(turn) = s.entries.pop_front() {
                removed_tokens += turn.approx_tokens;
            }
        }
        s.total_tokens = s.total_tokens.saturating_sub(removed_tokens);
        let approx_tokens = approx_tokens(&summary_text);
        s.summaries.push(SummaryBlock {
            text: summary_text,
            turns_covered: job.n,
            approx_tokens,
        });
        true
    }

    /// Test/diagnostic: number of summary blocks for `user_id`'s session.
    pub fn summary_count(&self, user_id: &str) -> usize {
        self.sessions.get(user_id).map(|s| s.summaries.len()).unwrap_or(0)
    }

    /// The active session id for `user_id`, if any (non-expired).
    pub fn session_id(&self, user_id: &str, now_unix: i64) -> Option<String> {
        match self.sessions.get(user_id) {
            Some(s) if !self.is_expired(s, now_unix) => Some(s.session_id.clone()),
            _ => None,
        }
    }

    /// Remove and return `user_id`'s session state (entries + metadata) for
    /// downstream flush (CONV-04). Returns `None` if no session exists.
    pub fn close_session(&mut self, user_id: &str) -> Option<SessionState> {
        self.sessions.remove(user_id)
    }

    /// Sweep all sessions and remove any past the inactivity timeout, returning
    /// the closed `(user_id, SessionState)` pairs so the caller can flush them.
    pub fn cleanup_expired(&mut self, now_unix: i64) -> Vec<(String, SessionState)> {
        let expired: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| now_unix.saturating_sub(s.last_activity) >= self.session_timeout_secs)
            .map(|(k, _)| k.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|k| self.sessions.remove(&k).map(|s| (k, s)))
            .collect()
    }

    /// Number of active (tracked) sessions, including not-yet-swept expired ones.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

// ── Config defaults (tuned from CONV-01 capacity report) ────────────────────

/// Default max turn-pairs per session.
///
/// CONV-01 found gpt-oss:20b recalls a planted fact cleanly through ~57K tokens
/// (640 turn-pairs) on the raw path with no coherence collapse, so quality does
/// not bound this. The live Chord path is bounded by its ~30s agentic timeout,
/// which truncates responses past ~29K tokens (~320 pairs). 20 pairs matches the
/// existing `CONVERSATION_WINDOW` convention and keeps the buffer's own footprint
/// tiny relative to that ceiling; the token budget below is the real safety limit.
pub const DEFAULT_BUFFER_SIZE: usize = 20;
/// Default max tokens held in the buffer.
///
/// CONV-01 practical ceiling ≈ 29K tokens (the point where the live Chord path's
/// 30s timeout truncates). 8000 is ~28% of that, leaving ample headroom for the
/// ~900-token dynamic system prompt, tool results, and the current message while
/// keeping typical responses fast and far under the ~65K hard slot
/// (131072 ctx / 2 parallel slots).
pub const DEFAULT_TOKEN_BUDGET: usize = 8000;
/// Default inactivity timeout in seconds (30 minutes).
pub const DEFAULT_SESSION_TIMEOUT_SECS: i64 = 1800;

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_i64(key: &str, default: i64) -> i64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 1_000_000; // arbitrary base time

    fn buf() -> ConversationBuffer {
        // generous token budget so turn-count is the binding limit unless tested
        ConversationBuffer::new(3, 100_000, 1800)
    }

    #[test]
    fn push_then_get_context_returns_entry() {
        let mut b = buf();
        b.push("@a:x", "hi", "hello", T);
        let ctx = b.get_context("@a:x", T);
        assert_eq!(ctx.len(), 1);
        assert_eq!(ctx[0].user_message, "hi");
        assert_eq!(ctx[0].assistant_response, "hello");
    }

    #[test]
    fn fifo_eviction_by_turn_count() {
        let mut b = buf(); // max_turns = 3
        for i in 0..4 {
            b.push("@a:x", format!("u{i}"), format!("a{i}"), T);
        }
        let ctx = b.get_context("@a:x", T);
        assert_eq!(ctx.len(), 3, "buffer must stay at max_turns");
        assert_eq!(ctx[0].user_message, "u1", "oldest (u0) evicted");
        assert_eq!(ctx[2].user_message, "u3", "newest retained");
    }

    #[test]
    fn eviction_by_token_budget() {
        // Tight token budget; each pair ~ (40+40)/4 = 20 tokens. Budget 45 => keep ~2.
        let mut b = ConversationBuffer::new(100, 45, 1800);
        let forty = "x".repeat(40);
        for _ in 0..5 {
            b.push("@a:x", &forty, &forty, T);
        }
        let total: usize = b.get_context("@a:x", T).iter().map(|e| e.approx_tokens).sum();
        assert!(total <= 45, "token budget enforced, got {total}");
        assert!(!b.get_context("@a:x", T).is_empty(), "keeps at least one turn");
    }

    #[test]
    fn newest_turn_kept_even_if_over_budget() {
        let mut b = ConversationBuffer::new(100, 5, 1800); // tiny budget
        let big = "y".repeat(400); // ~100 tokens, exceeds budget alone
        b.push("@a:x", &big, &big, T);
        assert_eq!(b.get_context("@a:x", T).len(), 1, "most recent turn never dropped");
    }

    #[test]
    fn session_timeout_clears_context() {
        let mut b = buf();
        b.push("@a:x", "hi", "hello", T);
        assert!(b.is_session_active("@a:x", T));
        // get_context past the timeout returns empty
        let later = T + 1800;
        assert!(!b.is_session_active("@a:x", later));
        assert!(b.get_context("@a:x", later).is_empty());
    }

    #[test]
    fn session_active_just_before_timeout_boundary() {
        let mut b = buf(); // timeout 1800
        b.push("@a:x", "hi", "hello", T);
        // one second before the boundary: still active, context intact
        assert!(b.is_session_active("@a:x", T + 1799));
        assert_eq!(b.get_context("@a:x", T + 1799).len(), 1);
        // at the boundary: expired
        assert!(!b.is_session_active("@a:x", T + 1800));
    }

    #[test]
    fn short_messages_still_count_tokens() {
        // ceiling estimate: each "hi"/"yo" is 1 token, so a pair is >=1 and the
        // token budget can actually evict short-message sessions.
        let mut b = ConversationBuffer::new(100, 3, 1800);
        for _ in 0..10 {
            b.push("@a:x", "hi", "yo", T);
        }
        let total: usize = b.get_context("@a:x", T).iter().map(|e| e.approx_tokens).sum();
        assert!(total > 0, "short messages must accrue tokens");
        assert!(total <= 3, "token budget enforced for short messages, got {total}");
    }

    #[test]
    fn expired_then_push_starts_fresh_session() {
        let mut b = buf();
        let sid1 = b.push("@a:x", "hi", "hello", T);
        let sid2 = b.push("@a:x", "again", "yo", T + 1800); // past timeout
        assert_ne!(sid1, sid2, "new session id after timeout");
        let ctx = b.get_context("@a:x", T + 1800);
        assert_eq!(ctx.len(), 1, "fresh session holds only the new turn");
        assert_eq!(ctx[0].user_message, "again");
    }

    #[test]
    fn close_session_returns_entries_and_clears() {
        let mut b = buf();
        b.push("@a:x", "hi", "hello", T);
        b.push("@a:x", "more", "ok", T);
        let closed = b.close_session("@a:x").expect("session existed");
        assert_eq!(closed.entries.len(), 2);
        assert!(b.get_context("@a:x", T).is_empty(), "buffer cleared after close");
        assert!(b.close_session("@a:x").is_none(), "second close is None");
    }

    #[test]
    fn cleanup_expired_removes_stale_sessions() {
        let mut b = buf();
        b.push("@old:x", "hi", "hello", T);
        b.push("@new:x", "hi", "hello", T + 1800);
        let closed = b.cleanup_expired(T + 1800);
        assert_eq!(closed.len(), 1, "only the stale session swept");
        assert_eq!(closed[0].0, "@old:x");
        assert!(b.is_session_active("@new:x", T + 1800));
    }

    #[test]
    fn multiple_users_are_isolated() {
        let mut b = buf();
        b.push("@a:x", "a-msg", "a-resp", T);
        b.push("@b:x", "b-msg", "b-resp", T);
        let a = b.get_context("@a:x", T);
        let bb = b.get_context("@b:x", T);
        assert_eq!(a.len(), 1);
        assert_eq!(bb.len(), 1);
        assert_eq!(a[0].user_message, "a-msg");
        assert_eq!(bb[0].user_message, "b-msg");
    }

    #[test]
    fn matrix_user_id_with_special_chars() {
        let mut b = buf();
        let uid = "@claude.livetest:example.com";
        b.push(uid, "hi", "hello", T);
        assert_eq!(b.get_context(uid, T).len(), 1);
    }

    // ── CONV-03: context_messages construction ──────────────────────────────

    #[test]
    fn context_messages_empty_for_first_message() {
        // Before any turn is recorded, a user's context is empty — so the first
        // message produces a messages array with only system + current user.
        let b = buf();
        assert!(b.context_messages("@a:x", T).is_empty());
    }

    #[test]
    fn context_messages_orders_oldest_first_and_alternates() {
        let mut b = buf();
        b.push("@a:x", "u1", "a1", T);
        b.push("@a:x", "u2", "a2", T);
        let msgs = b.context_messages("@a:x", T);
        // two turn-pairs → four messages, oldest first, user/assistant alternating
        assert_eq!(msgs.len(), 4);
        let pairs: Vec<(&str, &str)> = msgs
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(pairs, vec![
            ("user", "u1"), ("assistant", "a1"),
            ("user", "u2"), ("assistant", "a2"),
        ]);
    }

    #[test]
    fn context_messages_second_turn_contains_first_exchange() {
        // Models the CONV-03 flow: after turn 1 is recorded, turn 2's request
        // context contains turn 1's user+assistant exchange.
        let mut b = buf();
        b.push("@a:x", "My cat is Quorble", "Nice to meet Quorble!", T);
        let ctx = b.context_messages("@a:x", T);
        assert_eq!(ctx.len(), 2);
        assert_eq!(ctx[0].content.as_deref(), Some("My cat is Quorble"));
        assert_eq!(ctx[1].content.as_deref(), Some("Nice to meet Quorble!"));
    }

    #[test]
    fn context_messages_respects_eviction_overflow() {
        // Overflow trimming still yields a valid (capped) messages array.
        let mut b = ConversationBuffer::new(2, 100_000, 1800); // max 2 turns
        for i in 0..5 {
            b.push("@a:x", format!("u{i}"), format!("a{i}"), T);
        }
        let msgs = b.context_messages("@a:x", T);
        assert_eq!(msgs.len(), 4, "2 retained turns → 4 messages");
        assert_eq!(msgs[0].content.as_deref(), Some("u3"), "oldest retained is u3");
    }

    // ── CONV-05: progressive summarization ──────────────────────────────────

    #[test]
    fn summarization_due_fires_at_threshold() {
        let mut b = ConversationBuffer::new(100, 1_000_000, 1800);
        // below threshold → None
        for i in 0..3 {
            b.push("@a:x", format!("u{i}"), format!("a{i}"), T);
        }
        assert!(b.summarization_due("@a:x", T, 4).is_none());
        // reach threshold of 4 → Some, compresses oldest half (2)
        b.push("@a:x", "u3", "a3", T);
        let job = b.summarization_due("@a:x", T, 4).expect("due");
        assert_eq!(job.n, 2);
        assert_eq!(job.turns.len(), 2);
        assert_eq!(job.turns[0].user_message, "u0", "oldest first");
        // threshold < 2 is a no-op guard
        assert!(b.summarization_due("@a:x", T, 1).is_none());
    }

    #[test]
    fn install_summary_replaces_oldest_turns() {
        let mut b = ConversationBuffer::new(100, 1_000_000, 1800);
        for i in 0..4 {
            b.push("@a:x", format!("u{i}"), format!("a{i}"), T);
        }
        let job = b.summarization_due("@a:x", T, 4).unwrap();
        let ok = b.install_summary(&job, "summary of u0-u1".into());
        assert!(ok);
        assert_eq!(b.get_context("@a:x", T).len(), 2, "2 verbatim turns remain");
        assert_eq!(b.summary_count("@a:x"), 1);
        // messages array: summary (system) first, then the 2 remaining turn-pairs
        let msgs = b.context_messages("@a:x", T);
        assert_eq!(msgs[0].role, "system");
        assert!(msgs[0].content.as_deref().unwrap().contains("summary of u0-u1"));
        assert_eq!(msgs[1].content.as_deref(), Some("u2"));
        assert_eq!(msgs.len(), 1 + 4);
    }

    #[test]
    fn install_summary_rejects_stale_job() {
        let mut b = ConversationBuffer::new(100, 1_000_000, 1800);
        for i in 0..4 {
            b.push("@a:x", format!("u{i}"), format!("a{i}"), T);
        }
        // wrong session id → rejected, no mutation
        let mut job = b.summarization_due("@a:x", T, 4).unwrap();
        job.session_id = "not-the-session".to_string();
        assert!(!b.install_summary(&job, "x".into()));
        assert_eq!(b.get_context("@a:x", T).len(), 4);
        assert_eq!(b.summary_count("@a:x"), 0);
    }

    #[test]
    fn install_summary_rejects_when_front_changed() {
        // Identity fence: if FIFO eviction changes the front between snapshot and
        // install, the (now stale) summary is discarded rather than mislabeling turns.
        let mut b = ConversationBuffer::new(4, 1_000_000, 1800); // max 4 turns
        for i in 0..4 {
            b.push("@a:x", format!("u{i}"), format!("a{i}"), T);
        }
        let job = b.summarization_due("@a:x", T, 4).unwrap(); // n=2, snapshot u0,u1
        // two more pushes → FIFO evicts u0,u1 (max 4); front is now u2,u3
        b.push("@a:x", "u4", "a4", T);
        b.push("@a:x", "u5", "a5", T);
        assert!(!b.install_summary(&job, "stale summary".into()), "front changed → rejected");
        assert_eq!(b.summary_count("@a:x"), 0);
        assert_eq!(b.get_context("@a:x", T).len(), 4);
    }

    #[test]
    fn multiple_summarization_rounds_accumulate() {
        let mut b = ConversationBuffer::new(100, 1_000_000, 1800);
        for i in 0..4 {
            b.push("@a:x", format!("u{i}"), format!("a{i}"), T);
        }
        let j1 = b.summarization_due("@a:x", T, 4).unwrap();
        assert!(b.install_summary(&j1, "summary1".into()));
        // add more turns, summarize again
        for i in 4..8 {
            b.push("@a:x", format!("u{i}"), format!("a{i}"), T);
        }
        let j2 = b.summarization_due("@a:x", T, 4).unwrap();
        assert!(b.install_summary(&j2, "summary2".into()));
        assert_eq!(b.summary_count("@a:x"), 2, "two summary blocks accumulate");
        let msgs = b.context_messages("@a:x", T);
        assert_eq!(msgs[0].content.as_deref().unwrap().contains("summary1"), true);
        assert_eq!(msgs[1].content.as_deref().unwrap().contains("summary2"), true);
    }

    #[test]
    fn summaries_carry_into_close_session() {
        let mut b = ConversationBuffer::new(100, 1_000_000, 1800);
        for i in 0..4 {
            b.push("@a:x", format!("u{i}"), format!("a{i}"), T);
        }
        let j = b.summarization_due("@a:x", T, 4).unwrap();
        b.install_summary(&j, "the gist".into());
        let closed = b.close_session("@a:x").unwrap();
        assert_eq!(closed.summaries.len(), 1);
        assert_eq!(closed.entries.len(), 2);
    }

    #[test]
    fn unknown_user_returns_empty_not_error() {
        let b = buf();
        assert!(b.get_context("@nobody:x", T).is_empty());
        assert!(!b.is_session_active("@nobody:x", T));
    }

    #[test]
    fn concurrent_pushes_do_not_panic() {
        use std::sync::{Arc, RwLock};
        let shared: SharedBuffer = Arc::new(RwLock::new(ConversationBuffer::new(50, 1_000_000, 1800)));
        let mut handles = Vec::new();
        for t in 0..8 {
            let s = Arc::clone(&shared);
            handles.push(std::thread::spawn(move || {
                for i in 0..20 {
                    s.write().unwrap().push("@a:x", format!("u{t}-{i}"), "r", T);
                }
            }));
        }
        for h in handles { h.join().unwrap(); }
        let ctx = shared.read().unwrap().get_context("@a:x", T);
        assert_eq!(ctx.len(), 50, "capacity held under concurrent writes");
    }
}
