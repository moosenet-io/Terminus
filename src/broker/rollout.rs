//! TMOD-06: health-gated blue-green rollout + rollback for a worker UPDATE.
//!
//! Before this item, [`crate::broker::control`]'s `register_verified_transport`
//! treated re-registering an ALREADY-PRESENT worker exactly like a first-ever
//! registration: connect + health-probe + `list()`-verify the new instance,
//! then [`crate::broker::routes::RouteTable::replace_worker`] straight over
//! the old one. That is safe against a worker that's dead *at registration
//! time* (the pre-flip gate refuses it), but not against one that passes the
//! gate and then goes bad moments later (a bad build that boots, answers one
//! health probe, then wedges) — the old, known-good instance was already
//! gone by the time that showed up.
//!
//! This module closes that gap with a blue-green flip: the new instance is
//! routed in atomically WHILE the previous instance's routes are retained as
//! rollback state, then watched over a bounded post-flip health window
//! before the previous instance is finally let go. States mirror the fleet's
//! `constellation-updater` gate/rollback pattern (stage a new version, gate
//! it, roll back on failure) — see this crate's sibling `constellation-updater`
//! repo for the model this follows:
//!
//! ```text
//! Staging ──(pre-flip gate: connect+health+list, done by the CALLER)──> flip
//!    flip ──(atomic swap, previous retained)──> Live
//!    Live ──(post-flip health window: N consecutive probes)──┬─> pass ─> RetiredPrevious
//!                                                             └─> fail ─> RolledBack
//! ```
//!
//! ## Flip: atomic, previous retained
//! [`rollout_worker`] flips via
//! [`RouteTable::replace_worker_with_rollback`] — ONE `rcu` swap that
//! installs `new_routes` for `worker_id` AND hands back whatever routes it
//! just displaced (the worker's previous instance, or empty on a first-ever
//! registration). A reader's `RouteTable::load()` never observes a mix of
//! old and new routes for the worker (TMOD-04's no-tearing guarantee, which
//! this item relies on rather than re-implements) — every in-flight call
//! either dispatches against the pre-flip snapshot (old instance) or a
//! post-flip one (new instance), never both.
//!
//! ## Post-flip health window: bounded, fail-closed on any flap
//! [`POST_FLIP_HEALTH_CHECKS`] consecutive [`WorkerTransport::health`]
//! probes, each individually bounded by
//! [`crate::broker::routes::HEALTH_PROBE_TIMEOUT`] (same budget every other
//! health check in this crate uses — a probe that hangs is a failed probe,
//! never a stall). ANY single failed probe in the window fails the whole
//! rollout — this is deliberately "N of N", not "N of M": a worker that
//! flaps during its own post-flip window is exactly the case this item
//! exists to catch, so treating a flap as a pass would defeat the point.
//!
//! ## Rollback: atomic, and safe against a concurrent deregister
//! On a failed window, [`RouteTable::restore_worker_if_unchanged`] restores
//! the previous instance's routes (or, if there was no previous instance,
//! removes the failed routes entirely — fail SAFE, never leave a route
//! pointing at a proven-bad instance) IN THE SAME ATOMIC SWAP that checks
//! the worker's routes are still exactly what this rollout flipped to. If
//! they're not — because `handle_deregister` removed the worker, or a
//! NEWER rollout already superseded this one, while this window was running
//! — the restore is a clean no-op: no orphaned `previous` snapshot is ever
//! written back over a state this rollout no longer owns.
//!
//! ## Flip-then-verify: the accepted in-window trade-off
//! This is a flip-THEN-verify model: the pre-flip gate
//! (`control::register_verified_transport`'s connect + bounded health probe
//! + `list()`-verify) makes the new instance healthy AT flip time, the
//! post-flip window then catches a subsequent regression and rolls back
//! atomically. A narrow window does exist where a call landing AFTER the
//! flip but BEFORE a rollback can hit the just-regressed new instance —
//! that is the accepted trade-off, not a bug: fully avoiding it would
//! require dual-routing one tool name to both the old and new instance
//! simultaneously and reconciling their answers, which the flat one-route-
//! per-name table cannot express. The window is short, the pre-flip gate
//! makes a healthy-at-flip instance the norm, and rollback is atomic.
//!
//! ## Degraded state (both old and new unhealthy)
//! If a rollback's restored "previous" instance turns out to *also* be
//! unhealthy (e.g. both instances share a common outage), this module does
//! not paper over that: `RouteTable`'s per-call/per-list live health check
//! (unchanged from TMOD-04) means every subsequent `tools/call`/`tools/list`
//! against that worker still answers a clean "unavailable" rather than
//! silently routing to (or claiming success from) a dead instance. Rollback
//! restores the LAST-KNOWN-GOOD route identity, not a health guarantee.

use std::sync::Arc;
use std::time::Duration;

use super::routes::{RouteTable, WorkerRoute, HEALTH_PROBE_TIMEOUT};
use super::transport::WorkerTransport;

/// Rollout state machine. Mirrors `constellation-updater`'s gate/rollback
/// states:
/// - `Staging` — the new instance is up but not yet routed to any traffic;
///   this state is owned by the CALLER (`control::register_verified_transport`'s
///   connect + bounded health probe + `list()`-verify), not this module —
///   included here only so the full lifecycle is named in one place.
/// - `Live` — the new instance has been flipped in and is currently serving
///   (whether or not its post-flip window has finished).
/// - `RolledBack` — the post-flip window failed; traffic is back on the
///   previous instance (or, with no previous instance, removed entirely).
/// - `RetiredPrevious` — the post-flip window passed; the previous instance
///   has been let go and the new instance is the sole route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutState {
    Staging,
    Live,
    RolledBack,
    RetiredPrevious,
}

/// Number of consecutive post-flip health probes the new instance must ALL
/// pass before its previous instance is retired. Fixed and small (not a
/// long wall-clock soak) so a rollout resolves quickly and tests stay fast
/// and deterministic — a genuinely flapping worker fails this window on its
/// first bad probe (fail-closed), it does not get "best of N" chances.
pub const POST_FLIP_HEALTH_CHECKS: usize = 3;

/// Interval between consecutive post-flip probes — long enough to give a
/// truly-wedging instance a moment to show it (rather than probing the same
/// instant three times back-to-back), short enough that a rollout resolves
/// well within a caller's request timeout.
pub const POST_FLIP_HEALTH_INTERVAL: Duration = Duration::from_millis(10);

/// The outcome of one [`rollout_worker`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutOutcome {
    pub worker_id: String,
    pub state: RolloutState,
    /// Number of routes actively serving `worker_id` after this rollout
    /// resolved (the new instance's route count on `RetiredPrevious`; the
    /// restored previous instance's route count, or 0, on `RolledBack`).
    pub active_route_count: usize,
}

/// Bounded liveness probe -- identical budget/semantics to
/// `crate::broker::routes`'s own `health_bounded`/`crate::broker::control`'s
/// `probe_health`: a worker that accepts a probe but never answers must not
/// be able to stall a rollout's health window.
async fn health_bounded(transport: &Arc<dyn WorkerTransport>) -> bool {
    matches!(tokio::time::timeout(HEALTH_PROBE_TIMEOUT, transport.health()).await, Ok(true))
}

/// Perform a health-gated blue-green rollout of `new_routes` for
/// `worker_id`.
///
/// Callers are expected to have ALREADY pre-flip-gated `new_routes`'s
/// transport(s) — connect + bounded health probe + `list()`-verify, exactly
/// what `control::register_verified_transport` does before calling this —
/// this function's own job starts at the flip, not before it. Every route in
/// `new_routes` must carry `worker_id` (same contract
/// [`RouteTable::replace_worker`] documents).
///
/// See this module's doc for the full flip/window/rollback contract.
pub async fn rollout_worker(routes: &RouteTable, worker_id: &str, new_routes: Vec<WorkerRoute>) -> RolloutOutcome {
    let worker_id_owned = worker_id.to_string();

    // Flip: atomic swap, previous instance retained as this rollout's
    // rollback state (empty on a first-ever registration -- nothing to roll
    // back to).
    let previous_routes = routes.replace_worker_with_rollback(worker_id, new_routes.clone());

    // Post-flip health window: bounded, fixed-count, fail-closed on the
    // FIRST failed probe (a flap during the window is exactly the failure
    // this item exists to catch).
    let healthy = post_flip_window_passes(&new_routes).await;

    if healthy {
        // Previous instance is simply dropped -- nothing left holding it, no
        // route-table mutation needed (it is already gone from the table
        // since the flip).
        return RolloutOutcome {
            worker_id: worker_id_owned,
            state: RolloutState::RetiredPrevious,
            active_route_count: new_routes.len(),
        };
    }

    // Roll back -- atomically, and ONLY if the worker's routes are still
    // exactly what this rollout just flipped to (nobody deregistered or
    // superseded it out from under this window). `restore_worker_if_unchanged`
    // is the single primitive for both cases:
    //  - previous instance existed  -> restore it (true rollback).
    //  - no previous instance       -> restore an EMPTY set (fail safe: a
    //    first-ever registration that fails its own post-flip window is
    //    removed entirely rather than left routed to a proven-bad instance).
    let restored = routes.restore_worker_if_unchanged(worker_id, &new_routes, previous_routes.clone());
    let active_route_count = if restored {
        previous_routes.len()
    } else {
        // Cancelled: the worker's routes had already changed (deregistered,
        // or a newer rollout won) -- report whatever is ACTUALLY there now
        // rather than guessing, so a caller logging this outcome sees
        // reality, not this rollout's stale intent.
        routes.load().all().filter(|r| r.worker_id == worker_id_owned).count()
    };

    RolloutOutcome { worker_id: worker_id_owned, state: RolloutState::RolledBack, active_route_count }
}

/// Run the fixed post-flip health window against every route's transport
/// (typically all routes from `new_routes` share one transport per
/// [`WorkerRoute::transport`]'s doc, but this probes each DISTINCT transport
/// present, in case a future worker ever advertises tools split across more
/// than one). Returns `true` only if every probe, across the whole window,
/// passed -- fail-closed on the first failure.
async fn post_flip_window_passes(new_routes: &[WorkerRoute]) -> bool {
    if new_routes.is_empty() {
        // Nothing to probe -- vacuously fine; a worker registering zero
        // routes is refused upstream (`AdminError::NoTools`/`EmptyCatalog`)
        // long before this module is ever reached.
        return true;
    }

    let mut transports: Vec<Arc<dyn WorkerTransport>> = Vec::new();
    for route in new_routes {
        if !transports.iter().any(|t| Arc::ptr_eq(t, &route.transport)) {
            transports.push(route.transport.clone());
        }
    }

    for i in 0..POST_FLIP_HEALTH_CHECKS {
        for transport in &transports {
            if !health_bounded(transport).await {
                return false;
            }
        }
        if i + 1 < POST_FLIP_HEALTH_CHECKS {
            tokio::time::sleep(POST_FLIP_HEALTH_INTERVAL).await;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::transport::TransportError;
    use crate::error::ToolError;
    use crate::registry::ToolInfo;
    use crate::tool::ToolOutput;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A stub [`WorkerTransport`] whose `health()` answers `true` for the
    /// first `fail_after` calls, then `false` forever after -- models an
    /// instance that passes its pre-flip gate and then wedges during (or
    /// after) the post-flip window.
    struct FlakyTransport {
        fail_after: usize,
        health_calls: AtomicUsize,
        text: String,
    }

    impl FlakyTransport {
        fn always_healthy(text: &str) -> Self {
            Self { fail_after: usize::MAX, health_calls: AtomicUsize::new(0), text: text.to_string() }
        }
        fn fails_after(fail_after: usize, text: &str) -> Self {
            Self { fail_after, health_calls: AtomicUsize::new(0), text: text.to_string() }
        }
        fn always_unhealthy(text: &str) -> Self {
            Self { fail_after: 0, health_calls: AtomicUsize::new(0), text: text.to_string() }
        }
    }

    #[async_trait::async_trait]
    impl WorkerTransport for FlakyTransport {
        async fn connect(&self) -> Result<(), TransportError> {
            Ok(())
        }
        async fn call(&self, _name: &str, _args: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput { text: self.text.clone(), structured: None })
        }
        async fn list(&self) -> Result<Vec<String>, TransportError> {
            Ok(vec![])
        }
        async fn health(&self) -> bool {
            let n = self.health_calls.fetch_add(1, Ordering::SeqCst);
            n < self.fail_after
        }
    }

    fn tool_info(name: &str) -> ToolInfo {
        ToolInfo { name: name.to_string(), description: String::new(), parameters: serde_json::json!({"type": "object"}) }
    }

    fn route(worker_id: &str, tool: &str, transport: &Arc<dyn WorkerTransport>) -> WorkerRoute {
        WorkerRoute { worker_id: worker_id.to_string(), transport: transport.clone(), tool: tool_info(tool) }
    }

    // ── A healthy new instance flips live; previous is retired ──────────

    #[tokio::test]
    async fn healthy_new_instance_flips_live_and_retires_previous() {
        let routes = RouteTable::new();
        let old: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::always_healthy("old"));
        routes.install(route("w1", "tool_a", &old));

        let new: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::always_healthy("new"));
        let new_routes = vec![route("w1", "tool_a", &new)];

        let outcome = rollout_worker(&routes, "w1", new_routes).await;
        assert_eq!(outcome.state, RolloutState::RetiredPrevious);
        assert_eq!(outcome.active_route_count, 1);

        let snap = routes.load();
        let out = crate::broker::routes::dispatch_call(&snap, "tool_a", serde_json::json!({}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.text, "new", "the NEW instance serves after a passing rollout");
    }

    // ── A new instance that fails the post-flip window rolls back ───────

    #[tokio::test]
    async fn failing_new_instance_is_rolled_back_to_previous() {
        let routes = RouteTable::new();
        let old: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::always_healthy("old"));
        routes.install(route("w1", "tool_a", &old));

        // Passes the "pre-flip gate" (a caller-side concern this test
        // doesn't model) but fails every post-flip probe.
        let new: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::always_unhealthy("new"));
        let new_routes = vec![route("w1", "tool_a", &new)];

        let outcome = rollout_worker(&routes, "w1", new_routes).await;
        assert_eq!(outcome.state, RolloutState::RolledBack);
        assert_eq!(outcome.active_route_count, 1);

        let snap = routes.load();
        let out = crate::broker::routes::dispatch_call(&snap, "tool_a", serde_json::json!({}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.text, "old", "a rolled-back worker serves its PREVIOUS instance");
    }

    // ── Previous serves throughout -- no dropped calls across the flip+rollback ─

    #[tokio::test]
    async fn previous_serves_throughout_flip_and_rollback_no_dropped_calls() {
        let routes = Arc::new(RouteTable::new());
        let old: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::always_healthy("old"));
        routes.install(route("w1", "tool_a", &old));

        let new: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::always_unhealthy("new"));
        let new_routes = vec![route("w1", "tool_a", &new)];

        // Fire concurrent calls against fresh snapshots while the rollout
        // (flip -> window -> rollback) runs. Every single one must succeed
        // with EITHER "old" (pre-flip / post-rollback) or "new" (briefly
        // live mid-window) -- never an error, never a torn/mixed result.
        let routes_for_rollout = routes.clone();
        let rollout_task =
            tokio::spawn(async move { rollout_worker(&routes_for_rollout, "w1", new_routes).await });

        let mut saw_error = false;
        for _ in 0..200 {
            let snap = routes.load();
            match crate::broker::routes::dispatch_call(&snap, "tool_a", serde_json::json!({})).await {
                Some(Ok(out)) => assert!(out.text == "old" || out.text == "new"),
                Some(Err(_)) => saw_error = true,
                None => panic!("tool_a must always have a route during a rollout"),
            }
            tokio::task::yield_now().await;
        }
        assert!(!saw_error, "no call should ever observe the worker as unavailable across a flip/rollback");

        let outcome = rollout_task.await.unwrap();
        assert_eq!(outcome.state, RolloutState::RolledBack);

        let final_snap = routes.load();
        let out = crate::broker::routes::dispatch_call(&final_snap, "tool_a", serde_json::json!({}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.text, "old");
    }

    // ── Both old+new unhealthy: rollback restores the last-known-good
    //    identity, but dispatch still cleanly refuses to route to it ──────

    #[tokio::test]
    async fn both_unhealthy_rolls_back_but_dispatch_stays_clean_unavailable() {
        let routes = RouteTable::new();
        let old: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::always_unhealthy("old"));
        routes.install(route("w1", "tool_a", &old));

        let new: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::always_unhealthy("new"));
        let new_routes = vec![route("w1", "tool_a", &new)];

        let outcome = rollout_worker(&routes, "w1", new_routes).await;
        assert_eq!(outcome.state, RolloutState::RolledBack, "the new instance's window still fails closed");

        // The table now points back at the previous instance (rollback
        // happened -- last-known-good identity restored)...
        let snap = routes.load();
        assert!(snap.get("tool_a").is_some(), "rollback restores the previous route, it does not delete it");
        // ...but a live call still gets a clean "unavailable", never a
        // silent success and never routed to the (also-dead) new instance.
        let res = crate::broker::routes::dispatch_call(&snap, "tool_a", serde_json::json!({})).await.unwrap();
        assert!(res.is_err(), "an unhealthy restored instance must still answer 'unavailable' on call");
    }

    // ── Flapping mid-window (passes once, then fails) is fail-closed ────

    #[tokio::test]
    async fn flapping_mid_window_is_treated_as_a_failure_not_averaged() {
        let routes = RouteTable::new();
        let old: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::always_healthy("old"));
        routes.install(route("w1", "tool_a", &old));

        // Passes exactly ONE post-flip probe, then fails -- with
        // POST_FLIP_HEALTH_CHECKS > 1 this must still roll back (fail
        // closed on ANY flap, not "majority of the window").
        assert!(POST_FLIP_HEALTH_CHECKS > 1);
        let new: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::fails_after(1, "new"));
        let new_routes = vec![route("w1", "tool_a", &new)];

        let outcome = rollout_worker(&routes, "w1", new_routes).await;
        assert_eq!(outcome.state, RolloutState::RolledBack, "a flap anywhere in the window must fail closed");
    }

    // ── No previous instance (first-ever registration): failure removes,
    //    it does not "roll back" to nothing ─────────────────────────────

    #[tokio::test]
    async fn first_ever_registration_that_fails_its_window_is_removed_not_left_dangling() {
        let routes = RouteTable::new();
        let new: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::always_unhealthy("new"));
        let new_routes = vec![route("fresh", "tool_a", &new)];

        let outcome = rollout_worker(&routes, "fresh", new_routes).await;
        assert_eq!(outcome.state, RolloutState::RolledBack);
        assert_eq!(outcome.active_route_count, 0);

        let snap = routes.load();
        assert!(snap.get("tool_a").is_none(), "a first-ever registration failing its window leaves no route behind");
    }

    #[tokio::test]
    async fn first_ever_registration_that_passes_its_window_goes_live() {
        let routes = RouteTable::new();
        let new: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::always_healthy("new"));
        let new_routes = vec![route("fresh", "tool_a", &new)];

        let outcome = rollout_worker(&routes, "fresh", new_routes).await;
        assert_eq!(outcome.state, RolloutState::RetiredPrevious);
        let snap = routes.load();
        assert!(snap.get("tool_a").is_some());
    }

    // ── A deregister mid-rollout cancels cleanly -- no orphaned `previous` ─

    #[tokio::test]
    async fn deregister_mid_rollout_cancels_cleanly_no_orphaned_previous() {
        let routes = Arc::new(RouteTable::new());
        let old: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::always_healthy("old"));
        routes.install(route("w1", "tool_a", &old));

        // The new instance fails its window (would normally trigger a
        // rollback to `old`) -- but the worker gets deregistered WHILE the
        // window is running.
        let new: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::always_unhealthy("new"));
        let new_routes = vec![route("w1", "tool_a", &new)];

        let routes_for_rollout = routes.clone();
        let rollout_task =
            tokio::spawn(async move { rollout_worker(&routes_for_rollout, "w1", new_routes).await });

        // Give the flip a moment to land, then deregister the worker
        // entirely while the post-flip window is still probing.
        tokio::time::sleep(Duration::from_millis(2)).await;
        routes.remove_worker("w1");

        let outcome = rollout_task.await.unwrap();
        assert_eq!(outcome.state, RolloutState::RolledBack, "the window still failed on its own terms");

        // The critical assertion: deregistration WINS. No `previous`
        // (old/"old") was resurrected over top of the intentional removal.
        let snap = routes.load();
        assert!(snap.get("tool_a").is_none(), "a mid-rollout deregister must not be undone by a late rollback");
    }

    // ── Concurrent rollouts: a newer flip's routes are never clobbered by
    //    an older rollout's stale rollback ──────────────────────────────

    #[tokio::test]
    async fn a_superseding_rollout_is_not_clobbered_by_an_older_rollouts_rollback() {
        let routes = Arc::new(RouteTable::new());
        let v1: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::always_healthy("v1"));
        routes.install(route("w1", "tool_a", &v1));

        // Rollout A: v1 -> v2, where v2 fails its window.
        let v2: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::always_unhealthy("v2"));
        let previous_a = routes.replace_worker_with_rollback("w1", vec![route("w1", "tool_a", &v2)]);
        assert_eq!(previous_a.len(), 1);

        // Before A's window resolves, a second, newer rollout (B) already
        // supersedes it: v2 -> v3 (say v3 is healthy).
        let v3: Arc<dyn WorkerTransport> = Arc::new(FlakyTransport::always_healthy("v3"));
        let previous_b = routes.replace_worker_with_rollback("w1", vec![route("w1", "tool_a", &v3)]);
        assert_eq!(previous_b.len(), 1, "B's own previous is v2, not v1");

        // Now A's (stale) rollback attempt runs: it still thinks the
        // current routes are v2 (what IT flipped to) and tries to restore
        // v1 -- but the table has already moved on to v3. This must be a
        // no-op.
        let a_new_routes = vec![route("w1", "tool_a", &v2)];
        let restored = routes.restore_worker_if_unchanged("w1", &a_new_routes, previous_a);
        assert!(!restored, "a superseded rollout's rollback must not clobber a newer rollout's live routes");

        let snap = routes.load();
        let out = crate::broker::routes::dispatch_call(&snap, "tool_a", serde_json::json!({}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.text, "v3", "the newer rollout's flip must survive an older rollout's stale rollback");
    }
}
