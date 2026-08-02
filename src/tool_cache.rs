//! TRTR-08 — a TTL result cache for high-traffic assistant tools.
//!
//! Operator requirement (2026-07-31): *"give them daily pull data caching so the tool
//! won't have to be slow for each run. these are the most common tools i will use with
//! lumina."*
//!
//! `news_*` and `weather` are the assistant's highest-traffic tools. Without a cache
//! every conversational "what's the news?" costs a live upstream round-trip, and once
//! the tool router runs in-process (TRTR-02) each turn ALSO costs a Chord inference
//! call — so upstream latency compounds on exactly the path the operator uses most.
//!
//! ## Design rules that matter
//! - **Opt-in.** A tool with no policy is never cached; behaviour is unchanged.
//! - **Stale-while-revalidate.** Past the soft TTL the cached value is returned
//!   IMMEDIATELY and a refresh happens off the critical path. The user waits on a
//!   slow upstream at most once, not every time it goes slow.
//! - **Errors are never cached as successes.** A failing upstream must not pin a
//!   failure for a day; it gets a short backoff instead so we also don't hammer it.
//! - **`fetched_at` travels with the value** so the caller can honestly say "as of
//!   09:15" rather than implying a stale reading is live. Anti-fabrication applies to
//!   freshness as much as to content.
//! - **Principal-scoped where the data is.** A per-user result (e.g. a user's own
//!   saved locations) must never be served to a different principal, so the key
//!   includes the principal for those tools.
//! - **Bounded.** LRU-ish eviction by oldest-fetch; a cache must never become an
//!   unbounded leak on a 400+ tool surface.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::sync::RwLock;

/// Default number of entries kept before the oldest are evicted.
const DEFAULT_CAPACITY: usize = 512;

/// How long a failed fetch suppresses re-attempts. Short on purpose: long enough to
/// stop hammering a down upstream, short enough that recovery is quick.
const FAILURE_BACKOFF: Duration = Duration::from_secs(60);

/// Caching policy for one tool (or tool-name prefix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachePolicy {
    /// Past this age the value is STALE: still served immediately, but a background
    /// refresh is triggered.
    pub soft_ttl: Duration,
    /// Past this age the value is unusable and the caller must fetch synchronously.
    pub hard_ttl: Duration,
    /// Whether the cache key must include the calling principal (user-scoped data).
    pub per_principal: bool,
}

impl CachePolicy {
    pub const fn new(soft_secs: u64, hard_secs: u64, per_principal: bool) -> Self {
        Self {
            soft_ttl: Duration::from_secs(soft_secs),
            hard_ttl: Duration::from_secs(hard_secs),
            per_principal,
        }
    }
}

/// The seed policy table. Prefix-matched, longest prefix wins.
///
/// Rationale per entry — these are freshness judgements, not arbitrary numbers:
/// - `news_` — headlines move over hours, not seconds. 15 min soft / 24 h hard gives
///   a "daily pull" that still refreshes through the day, and the hard bound means a
///   day-old headline is never presented as current.
/// - `weather` — current conditions genuinely change; 20 min soft is about the
///   resolution of the underlying data anyway. 6 h hard so a stale reading can never
///   masquerade as now.
/// - Severe-weather ALERTS are deliberately ABSENT from this table (see
///   `is_never_cached`): a stale storm warning is worse than a slow one.
///
/// `per_principal` is a PRIVACY judgement, not a freshness one:
/// - `news_` — the same public headlines for everybody; the args fully determine
///   the answer, so one shared entry is correct.
/// - `weather` — **per-principal (TRTR-05)**. It looks like a pure function of
///   its arguments, and for an explicit location it is. But with the location
///   OMITTED the answer depends on WHO asked: the operator's answer is resolved
///   from the operator's calendar/home-work routine and is attributed out loud
///   ("using <place> — from your calendar (<event summary>)"), while a guest is
///   asked which place they mean. Sharing one entry keyed on `weather` + `{}`
///   would hand the operator's cached, attributed answer straight to the next
///   guest who asks — routing round the caller gate in `crate::weather` entirely,
///   since a cache hit never reaches the tool. The cost is a lower hit rate
///   across household members; the alternative is a disclosure.
const SEED_POLICY: &[(&str, CachePolicy)] = &[
    ("news_", CachePolicy::new(900, 86_400, false)),
    ("weather", CachePolicy::new(1_200, 21_600, true)),
];

/// Tools that must NEVER be served from cache regardless of any prefix policy.
///
/// Safety-relevant freshness beats latency every time. A severe-weather alert exists
/// precisely to be timely; serving yesterday's "all clear" is a failure mode with real
/// consequences for someone deciding whether to travel.
/// Tools named EXACTLY here are never cached, whatever any prefix policy says.
///
/// MUSEL-LIVE: the name-shape rule below (`alert`/`severe`/`warning`) encodes
/// one reason freshness is non-negotiable — safety. It is not the only one. A
/// LIVE-STATE read is worthless stale for a different reason: it answers "what
/// is true right now", and a cached "right now" is simply a lie. No honest name
/// for `media_now_playing` contains "alert", and renaming it to smuggle it past
/// a substring match would be a naming trick standing in for a decision — so
/// the RULE is extended instead, deliberately and by exact name.
///
/// Being an exact-name list rather than another substring makes the guarantee
/// survive an unrelated future edit: if somebody later adds a `("media_", ...)`
/// prefix policy to [`SEED_POLICY`], this list still wins.
pub const NEVER_CACHED_TOOLS: &[&str] = &[
    // What is playing on Plex at this instant, and who is home to be watching
    // it. A stale answer here is not merely unhelpful, it is wrong about the
    // household — and it would also route around the per-caller entitlement
    // gate in `crate::media::now_playing`, since a cache hit never reaches the
    // tool (the same hazard already documented for `weather` above, except that
    // no per-principal key can fix it: the data is live, not just private).
    "media_now_playing",
];

pub fn is_never_cached(tool: &str) -> bool {
    NEVER_CACHED_TOOLS.contains(&tool)
        || tool.contains("alert")
        || tool.contains("severe")
        || tool.contains("warning")
}

/// Resolve the policy governing `tool`, if any.
pub fn policy_for(tool: &str) -> Option<CachePolicy> {
    if is_never_cached(tool) {
        return None;
    }
    SEED_POLICY
        .iter()
        .filter(|(prefix, _)| tool.starts_with(prefix))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, p)| *p)
}

#[derive(Debug, Clone)]
struct Entry {
    value: String,
    /// Unix seconds. Stored as an absolute stamp (not `Instant`) so it can be
    /// reported to the caller as "as of ..." and compared across a restart-free
    /// process lifetime without monotonic-clock plumbing.
    fetched_at: u64,
    /// Set while a background refresh is in flight, so N concurrent stale hits
    /// trigger ONE refresh rather than a thundering herd.
    refreshing: bool,
    /// Unix seconds until which fetches are suppressed after a failure.
    backoff_until: u64,
}

/// What a lookup found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// Fresh hit — use it, no refresh needed.
    Fresh { value: String, fetched_at: u64 },
    /// Stale hit — use it NOW, and refresh in the background. `claim` is true for
    /// exactly one caller, which is the one that should perform the refresh.
    Stale { value: String, fetched_at: u64, claim: bool },
    /// Nothing usable — the caller must fetch synchronously.
    Miss,
    /// A recent fetch failed and we are backing off; the caller should fetch anyway
    /// only if it has no alternative (there is no cached value to serve).
    Backoff,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A bounded TTL cache over tool results.
#[derive(Debug, Clone)]
pub struct ToolCache {
    inner: Arc<RwLock<HashMap<String, Entry>>>,
    capacity: usize,
}

impl Default for ToolCache {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl ToolCache {
    pub fn with_capacity(capacity: usize) -> Self {
        Self { inner: Arc::new(RwLock::new(HashMap::new())), capacity: capacity.max(1) }
    }

    /// Build the cache key.
    ///
    /// Arguments are NORMALIZED through `serde_json` canonicalization (sorted keys via
    /// `BTreeMap` ordering, whitespace removed) so `{"a":1, "b":2}` and
    /// `{ "b":2 , "a":1 }` hit the same entry. Without this the cache would silently
    /// never hit for a model that varies its argument formatting between turns —
    /// which is exactly what an LLM does.
    pub fn key(tool: &str, args: &Value, principal: Option<&str>, per_principal: bool) -> String {
        let canonical = canonicalize(args);
        match (per_principal, principal) {
            (true, Some(p)) => format!("{tool}\u{1f}{p}\u{1f}{canonical}"),
            // A per-principal tool with NO principal is keyed to a sentinel rather
            // than sharing the anonymous bucket with a named user's data.
            (true, None) => format!("{tool}\u{1f}<anon>\u{1f}{canonical}"),
            (false, _) => format!("{tool}\u{1f}{canonical}"),
        }
    }

    /// Look up `key` under `policy`.
    pub async fn get(&self, key: &str, policy: CachePolicy) -> Lookup {
        let now = now_secs();
        // Fast path under a read lock.
        {
            let map = self.inner.read().await;
            if let Some(e) = map.get(key) {
                let age = now.saturating_sub(e.fetched_at);
                if age < policy.soft_ttl.as_secs() {
                    return Lookup::Fresh { value: e.value.clone(), fetched_at: e.fetched_at };
                }
                if age < policy.hard_ttl.as_secs() {
                    // Stale but usable. If a recent refresh FAILED we are backing off:
                    // still SERVE the value (it is better than nothing), but do not let
                    // anyone claim another refresh yet — otherwise every stale hit
                    // re-hammers a down upstream, which is what the backoff exists to
                    // prevent (round-2 review finding).
                    if e.backoff_until > now {
                        return Lookup::Stale {
                            value: e.value.clone(),
                            fetched_at: e.fetched_at,
                            claim: false,
                        };
                    }
                    // Claim the refresh only if nobody else has.
                    if e.refreshing {
                        return Lookup::Stale {
                            value: e.value.clone(),
                            fetched_at: e.fetched_at,
                            claim: false,
                        };
                    }
                    // fall through to take the write lock and claim it
                } else if e.backoff_until > now {
                    return Lookup::Backoff;
                } else {
                    return Lookup::Miss;
                }
            } else {
                return Lookup::Miss;
            }
        }
        // Slow path: claim the refresh for exactly one caller.
        let mut map = self.inner.write().await;
        match map.get_mut(key) {
            Some(e) => {
                let age = now.saturating_sub(e.fetched_at);
                if age >= policy.hard_ttl.as_secs() {
                    return Lookup::Miss;
                }
                if e.backoff_until > now {
                    return Lookup::Stale {
                        value: e.value.clone(),
                        fetched_at: e.fetched_at,
                        claim: false,
                    };
                }
                let claim = !e.refreshing;
                e.refreshing = true;
                Lookup::Stale { value: e.value.clone(), fetched_at: e.fetched_at, claim }
            }
            None => Lookup::Miss,
        }
    }

    /// Store a SUCCESSFUL result.
    pub async fn put(&self, key: &str, value: String) {
        let mut map = self.inner.write().await;
        map.insert(
            key.to_string(),
            Entry { value, fetched_at: now_secs(), refreshing: false, backoff_until: 0 },
        );
        Self::evict_if_needed(&mut map, self.capacity);
    }

    /// Record a FAILED fetch.
    ///
    /// Never stores the error as a value — a cached error would be served as though it
    /// were data for the whole TTL. It only sets a short backoff, and it explicitly
    /// PRESERVES any existing good value so a failed background refresh degrades to
    /// "slightly staler" rather than poisoning the entry.
    pub async fn record_failure(&self, key: &str) {
        let mut map = self.inner.write().await;
        let until = now_secs() + FAILURE_BACKOFF.as_secs();
        match map.get_mut(key) {
            Some(e) => {
                e.refreshing = false;
                e.backoff_until = until;
            }
            None => {
                map.insert(
                    key.to_string(),
                    Entry {
                        value: String::new(),
                        // An epoch stamp so this placeholder is always "hard-expired"
                        // and can never be served as a value.
                        fetched_at: 0,
                        refreshing: false,
                        backoff_until: until,
                    },
                );
            }
        }
        Self::evict_if_needed(&mut map, self.capacity);
    }

    /// Release a refresh claim that will not be acted on, so the entry is not stranded
    /// as permanently "refreshing" until hard expiry.
    pub async fn clear_refreshing(&self, key: &str) {
        if let Some(e) = self.inner.write().await.get_mut(key) {
            e.refreshing = false;
        }
    }

    /// Drop a single entry (operator-forced refresh).
    pub async fn invalidate(&self, key: &str) {
        self.inner.write().await.remove(key);
    }

    /// Drop every entry whose tool name matches `prefix`.
    pub async fn invalidate_prefix(&self, prefix: &str) -> usize {
        let mut map = self.inner.write().await;
        let before = map.len();
        map.retain(|k, _| !k.starts_with(prefix));
        before - map.len()
    }

    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// Evict oldest-fetched entries until within capacity.
    fn evict_if_needed(map: &mut HashMap<String, Entry>, capacity: usize) {
        if map.len() <= capacity {
            return;
        }
        let mut by_age: Vec<(String, u64)> =
            map.iter().map(|(k, e)| (k.clone(), e.fetched_at)).collect();
        by_age.sort_by_key(|(_, t)| *t);
        let excess = map.len() - capacity;
        for (k, _) in by_age.into_iter().take(excess) {
            map.remove(&k);
        }
    }
}

/// Canonical JSON: object keys sorted, no incidental whitespace. `serde_json::Value`
/// already stores maps in a `BTreeMap` when the `preserve_order` feature is off, so
/// `to_string` is deterministic; this wrapper makes the intent explicit and gives one
/// place to harden if that assumption ever changes.
fn canonicalize(args: &Value) -> String {
    fn norm(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut out = serde_json::Map::new();
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                for k in keys {
                    out.insert(k.clone(), norm(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
            Value::String(s) => Value::String(s.trim().to_string()),
            other => other.clone(),
        }
    }
    norm(args).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn policy_covers_the_two_high_traffic_families() {
        assert!(policy_for("news_headlines").is_some());
        assert!(policy_for("news_search").is_some());
        assert!(policy_for("weather").is_some());
    }

    /// TRTR-05: with the location OMITTED, `weather`'s answer depends on WHO
    /// asked — the operator's is resolved from their calendar/routine and says
    /// so. A shared cache entry would serve that answer to the next guest who
    /// asks the identical question, and a cache hit never reaches the tool's own
    /// caller gate, so this key is the only thing standing in the way.
    #[test]
    fn weather_is_cached_per_principal_so_one_answer_is_never_reused_across_people() {
        let p = policy_for("weather").expect("weather is cached");
        assert!(p.per_principal, "weather answers are principal-specific");
        let same_question = json!({});
        let operator = ToolCache::key("weather", &same_question, Some("operator-identity"), p.per_principal);
        let guest = ToolCache::key("weather", &same_question, Some("guest-identity"), p.per_principal);
        let anon = ToolCache::key("weather", &same_question, None, p.per_principal);
        assert_ne!(operator, guest);
        assert_ne!(operator, anon);
    }

    /// TERM-599: two PEOPLE behind one service principal must not share a
    /// per-principal cache entry.
    ///
    /// The router keys this cache on `CallerKey::storage_key()`, which is
    /// `svc:<principal>` for a service-scoped caller and
    /// `svc:<principal>#person:<person>` once a person is verified. Keying on the
    /// bare principal name instead would give every family member the same bucket:
    /// the first person to ask "what's on my calendar" would populate an entry the
    /// next person is then served. That is a disclosure strictly WORSE than the
    /// uniform service-scoping it replaces, which is why this has to hold from the
    /// moment per-person dispatch works, not afterwards.
    #[test]
    fn two_people_behind_one_principal_do_not_share_a_cache_entry() {
        let p = policy_for("weather").expect("weather is cached per principal");
        assert!(p.per_principal, "this test is meaningless unless the tool is per-principal");
        let q = json!({"q": "today"});

        // Identities derived the way PRODUCTION derives them — through
        // `CallerKey`, not hand-written strings. An earlier version of this test
        // asserted literals, which proved only that `ToolCache::key` is injective
        // and would have passed even while the caller computed the wrong identity.
        let alice = ck_identity(Some("alice"));  // pii-test-fixture: invented names
        let bob = ck_identity(Some("bob"));      // pii-test-fixture
        let service = ck_identity(None);

        let ka = ToolCache::key("weather", &q, Some(&alice), p.per_principal);
        let kb = ToolCache::key("weather", &q, Some(&bob), p.per_principal);
        let ks = ToolCache::key("weather", &q, Some(&service), p.per_principal);

        assert_ne!(ka, kb, "two people must not share a cache bucket");
        assert_ne!(ka, ks, "a person must not read the shared service entry");
        assert_ne!(kb, ks);
    }

    /// Derive a cache identity the way `dispatch_tool` does: through `CallerKey`.
    fn ck_identity(person: Option<&str>) -> String {
        match person {
            Some(p) => crate::locations::CallerKey::for_person("lumina", p)
                .expect("a named person must key")
                .storage_key(),
            None => crate::locations::CallerKey::for_principal_name("lumina")
                .expect("a service principal must key")
                .storage_key(),
        }
    }

    /// THE BUG THIS FIXES. A REFUSED assertion must not share the service bucket.
    ///
    /// The first version of the router change derived the cache identity as
    /// `caller_key_for(..).or(principal)`. `caller_key_for` returns `None` for a
    /// rejected assertion, so it fell through to the bare principal — and a claim
    /// we had just refused to believe could read, and populate, the same cached
    /// data as a legitimate service-scoped caller. That is "an attempted identity
    /// indistinguishable from no identity", one layer below where the rest of this
    /// work rules it out.
    #[test]
    fn a_refused_assertion_does_not_share_the_service_cache_bucket() {
        let p = policy_for("weather").unwrap();
        let q = json!({"q": "today"});
        let service = ck_identity(None);

        let refused = ToolCache::key("weather", &q, Some("<refused>"), p.per_principal);
        let svc = ToolCache::key("weather", &q, Some(&service), p.per_principal);
        let alice = ToolCache::key("weather", &q, Some(&ck_identity(Some("alice"))), p.per_principal); // pii-test-fixture

        assert_ne!(refused, svc, "a refused claim must not read service-scoped cache");
        assert_ne!(refused, alice, "nor any person's");
    }

    /// POSITIVE CONTROL for the above: the SAME identity asking the SAME question
    /// still hits one key, so the partitioning has not simply disabled caching.
    #[test]
    fn the_same_person_asking_twice_still_shares_one_entry() {
        let p = policy_for("weather").unwrap();
        let q = json!({"q": "today"});
        let alice = ck_identity(Some("alice")); // pii-test-fixture
        let a = ToolCache::key("weather", &q, Some(&alice), p.per_principal);
        let b = ToolCache::key("weather", &q, Some(&alice), p.per_principal);
        assert_eq!(a, b);
    }

    #[test]
    fn uncached_tools_are_opt_in_only() {
        // The default for anything without a policy is NO caching — behaviour is
        // unchanged for the other ~400 tools.
        assert!(policy_for("pve__get_nodes").is_none());
        assert!(policy_for("utc_now").is_none());
        assert!(policy_for("ledger_recent").is_none());
    }

    #[test]
    fn severe_weather_is_never_cached() {
        // Freshness beats latency for safety-relevant data: serving a stale
        // "all clear" is worse than serving a slow live answer.
        assert!(policy_for("weather_alerts").is_none());
        assert!(policy_for("weather_severe_watch").is_none());
        assert!(policy_for("storm_warning").is_none());
    }

    #[test]
    fn key_normalizes_argument_formatting() {
        // An LLM varies its argument formatting between turns. Without
        // canonicalization the cache would silently never hit.
        let a = json!({"location": "Omaha", "days": 3});
        let b = json!({"days": 3, "location": "Omaha"});
        assert_eq!(
            ToolCache::key("weather", &a, None, false),
            ToolCache::key("weather", &b, None, false)
        );
    }

    #[test]
    fn key_trims_incidental_whitespace_in_string_args() {
        let a = json!({"location": "Omaha"});
        let b = json!({"location": "  Omaha  "});
        assert_eq!(
            ToolCache::key("weather", &a, None, false),
            ToolCache::key("weather", &b, None, false)
        );
    }

    #[test]
    fn per_principal_keys_do_not_collide_across_users() {
        let args = json!({});
        let a = ToolCache::key("my_locations", &args, Some("alice"), true);
        let b = ToolCache::key("my_locations", &args, Some("guest"), true);
        assert_ne!(a, b, "user-scoped results must never be shared between principals");
        // And an unidentified caller gets its own bucket, not a named user's.
        let anon = ToolCache::key("my_locations", &args, None, true);
        assert_ne!(anon, a);
        assert_ne!(anon, b);
    }

    #[test]
    fn non_per_principal_keys_are_shared() {
        let args = json!({"country": "us"});
        assert_eq!(
            ToolCache::key("news_headlines", &args, Some("alice"), false),
            ToolCache::key("news_headlines", &args, Some("guest"), false),
            "world-scoped data should be shared, that is the point of the cache"
        );
    }

    #[tokio::test]
    async fn a_fresh_entry_is_served_without_refresh() {
        let c = ToolCache::default();
        let p = CachePolicy::new(900, 86_400, false);
        c.put("k", "payload".into()).await;
        match c.get("k", p).await {
            Lookup::Fresh { value, .. } => assert_eq!(value, "payload"),
            other => panic!("expected Fresh, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_stale_entry_is_served_immediately_and_claimed_once() {
        let c = ToolCache::default();
        // soft_ttl 0 => immediately stale; hard_ttl large => still usable.
        let p = CachePolicy::new(0, 86_400, false);
        c.put("k", "payload".into()).await;

        // First stale hit claims the refresh...
        match c.get("k", p).await {
            Lookup::Stale { value, claim, .. } => {
                assert_eq!(value, "payload", "stale value must still be SERVED, not withheld");
                assert!(claim, "the first stale caller should claim the refresh");
            }
            other => panic!("expected Stale, got {other:?}"),
        }
        // ...and a concurrent one does NOT (no thundering herd).
        match c.get("k", p).await {
            Lookup::Stale { claim, .. } => assert!(!claim, "only one caller may refresh"),
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_hard_expired_entry_is_a_miss() {
        let c = ToolCache::default();
        c.put("k", "payload".into()).await;
        // Both TTLs zero => past hard expiry.
        assert_eq!(c.get("k", CachePolicy::new(0, 0, false)).await, Lookup::Miss);
    }

    #[tokio::test]
    async fn a_failed_refresh_preserves_the_last_good_value() {
        // The load-bearing property: a background refresh that fails must degrade to
        // "slightly staler", never poison the entry with an error.
        let c = ToolCache::default();
        let p = CachePolicy::new(0, 86_400, false);
        c.put("k", "good".into()).await;
        let _ = c.get("k", p).await; // marks refreshing
        c.record_failure("k").await;
        match c.get("k", p).await {
            Lookup::Stale { value, claim, .. } => {
                assert_eq!(value, "good", "last-good value must survive a failed refresh");
                // Round-2 review: the ORIGINAL assertion here demanded an IMMEDIATE
                // retry, which meant every stale hit re-hammered a failing upstream.
                // During the backoff window the value is still SERVED but no refresh
                // may be claimed.
                assert!(!claim, "no refresh may be claimed while backing off");
            }
            other => panic!("expected Stale with the good value, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_error_is_never_stored_as_a_value() {
        let c = ToolCache::default();
        c.record_failure("k").await;
        // No prior good value => must NOT be served as data.
        let got = c.get("k", CachePolicy::new(900, 86_400, false)).await;
        assert!(
            matches!(got, Lookup::Miss | Lookup::Backoff),
            "a failure must never become a served value, got {got:?}"
        );
    }

    #[tokio::test]
    async fn capacity_is_bounded_by_eviction() {
        let c = ToolCache::with_capacity(3);
        for i in 0..10 {
            c.put(&format!("k{i}"), format!("v{i}")).await;
        }
        assert!(c.len().await <= 3, "cache must stay bounded, got {}", c.len().await);
    }

    #[tokio::test]
    async fn invalidate_prefix_forces_a_refresh_for_a_family() {
        let c = ToolCache::default();
        c.put("news_headlines\u{1f}{}", "a".into()).await;
        c.put("news_search\u{1f}{}", "b".into()).await;
        c.put("weather\u{1f}{}", "c".into()).await;
        let dropped = c.invalidate_prefix("news_").await;
        assert_eq!(dropped, 2);
        assert_eq!(c.len().await, 1, "only the news family should be dropped");
    }

    #[tokio::test]
    async fn a_missing_key_is_a_miss_not_a_panic() {
        let c = ToolCache::default();
        assert_eq!(c.get("nope", CachePolicy::new(900, 86_400, false)).await, Lookup::Miss);
    }
}
