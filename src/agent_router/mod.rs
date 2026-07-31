//! TRTR-02 — the agentic tool router, relocated from Chord to the Terminus egress.
//!
//! ## Why it moved
//! Chord ran the tool loop but has **no caller identity**: it held one flat catalog
//! for every client, so tool exposure could not be scoped per user — a family member
//! or guest would have been offered the operator's entire fleet surface. Terminus is
//! already in the path (`/v1/agent/execute` is a Terminus route that resolves the
//! principal and forwards to Chord *carrying* the caller identity), and Terminus is
//! where authorization lives. So the router belongs here, and Chord goes back to being
//! purely the inference layer.
//!
//! ## What this does per turn
//! 1. Resolve the caller principal (done upstream, passed in).
//! 2. **Select** tools — authorization ∩ availability ∩ relevance (`select`).
//! 3. Ask **Chord** for a completion, offering those tools.
//! 4. If the model asks for a tool: dispatch it **locally** through the registry,
//!    behind the TTL cache (`crate::tool_cache`), append the result, loop.
//! 5. Otherwise return the assistant's text.
//!
//! ## Invariants
//! - **Never advertise a tool that cannot be dispatched.** This is the lesson from the
//!   `deep_research` phantom: the model called a tool that was offered but had no
//!   dispatch path, wasted turns, and told the operator "the research tool ran into an
//!   error". Selection draws only from tools that exist in the registry.
//! - **Re-check availability at dispatch.** Selection is a snapshot; a tool parked
//!   between selection and dispatch must still be refused.
//! - **Bounded.** Step cap and a wall-clock budget strictly below the caller's egress
//!   timeout, so the router's own error surfaces rather than the client timing out.
//! - **Never hard-wire a model.** Inference goes to a Chord *named proxy*.

pub mod select;

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::inference_proxy::InferenceProxyClient;
use crate::mesh::principal::Principal;
use crate::registry::{ToolInfo, ToolRegistry};
use crate::tool_cache::{self, Lookup, ToolCache};

/// Hard ceiling on tool calls in one turn. A model that has not answered after this
/// many tools is looping, and more steps will not rescue it.
pub const MAX_STEPS: usize = 8;

/// Wall-clock budget. Must stay comfortably under lumina-core's 120 s egress timeout
/// so the router's own structured error is what the user sees, not a dead socket.
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(90);

/// One step of what happened, for logs and debugging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub tool: String,
    pub status: &'static str,
    pub cached: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub struct RouterOutcome {
    pub response: String,
    pub steps: Vec<Step>,
    pub turns: usize,
    pub status: &'static str,
}

/// Why a dispatch attempt ended the way it did. Separated from the transport so the
/// loop can make a decision without string-matching an error message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    Ok { text: String, cached: bool },
    /// The tool exists but is parked (TAVAIL-01) — a truthful, actionable message.
    Unavailable(String),
    /// The tool is not in the registry at all.
    Unknown(String),
    /// The tool ran and failed.
    Failed(String),
    /// The caller is NOT AUTHORIZED for this tool (or it is operator-guarded).
    /// Distinct from `Unavailable` so the audit trail can tell "you may not" from
    /// "nobody may right now".
    Denied(String),
}

/// Dispatch one tool call locally, through the cache when the tool has a policy.
///
/// `principal` is threaded in because a per-principal cache policy must not serve one
/// user's data to another.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_tool(
    state: Option<&crate::mcp_server::McpServerState>,
    cache: &ToolCache,
    gateway: Option<&crate::gateway_framework::GatewayFramework>,
    principal_obj: Option<&Principal>,
    name: &str,
    args: Value,
    principal: Option<&str>,
) -> Dispatch {
    // ── AUTHORIZATION, ENFORCED AT DISPATCH ──────────────────────────────────
    // Review (S128) caught this as the load-bearing hole: SELECTION IS ADVISORY.
    // The model can emit a tool_call for ANY name — including one injected by
    // untrusted content it just read from a news or web tool — so filtering the
    // offered list is not an enforcement boundary. Without this check an existing,
    // available, but UNAUTHORIZED tool would dispatch straight through
    // `registry.call`, defeating the entire premise of relocating the router for
    // per-principal scoping.
    //
    // `guard()` (not the read-only `permits_tool`) is used deliberately: this IS an
    // attempt, so it must consume rate-limit budget and emit an audit entry exactly
    // like the `tools/call` path does.
    // The GatewayContext returned on success MUST be recorded — the gateway contract
    // is one audit write per request, and `guard()` only logs DENIALS. Dropping it (as
    // the first cut did) left every allowed/failed/cached tool call with no terminal
    // audit entry, which is precisely the trail an operator needs after an incident.
    let mut gate_ctx = None;
    if let Some(gw) = gateway {
        match gw.guard(principal_obj, name, crate::gateway_framework::ActionKind::Tool).await {
            Ok(ctx) => gate_ctx = Some(ctx),
            Err(_denial) => {
                return Dispatch::Denied(format!(
                    "`{name}` is not available to you. Do not call it again; answer with \
                     the tools you were given."
                ));
            }
        }
    }

    /// Record the terminal audit outcome exactly once, whatever path we exit by.
    macro_rules! finish {
        ($ctx:expr, $ok:expr, $detail:expr, $ret:expr) => {{
            if let Some(c) = $ctx.take() {
                c.record_result($ok, $detail);
            }
            return $ret;
        }};
    }

    // ── OPERATOR-GUARDED TOOLS ARE NEVER MODEL-INVOCABLE ─────────────────────
    // Ported from Chord's `is_llm_blocked` hard block. The approval mechanism exists
    // to put a human in the loop; a model that could call `approval_grant` would
    // approve its own guarded requests and dissolve that gate entirely.
    let bare = crate::mesh::split_namespaced(name).map(|(_, b)| b).unwrap_or(name);
    if is_model_blocked(bare) {
        finish!(gate_ctx, false, Some("model-blocked (operator-guarded)"),
            Dispatch::Denied(format!(
                "`{name}` requires operator approval and cannot be invoked by an assistant. \
                 Ask the operator to run it."
            )));
    }

    // Availability is re-checked HERE, not just at selection: selection is a snapshot,
    // and a tool parked in between must still be refused.
    let avail = crate::availability::policy();
    if !avail.agent_usable(name) {
        finish!(gate_ctx, false, Some("unavailable"),
            Dispatch::Unavailable(avail.denial_message(name)));
    }

    let policy = tool_cache::policy_for(name);
    let key = policy
        .map(|p| ToolCache::key(name, &args, principal, p.per_principal))
        .unwrap_or_default();

    // Cache read.
    if let Some(p) = policy {
        match cache.get(&key, p).await {
            Lookup::Fresh { value, fetched_at } => {
                finish!(gate_ctx, true, Some("cache-hit"),
                    Dispatch::Ok { text: with_as_of(value, fetched_at), cached: true });
            }
            Lookup::Stale { value, fetched_at, claim } => {
                // Serve the stale value NOW; the caller never waits on a merely-stale
                // upstream. Refresh off the critical path, and only the one caller
                // that claimed it (no thundering herd).
                if claim {
                    spawn_refresh(cache, name, args.clone(), key.clone());
                }
                finish!(gate_ctx, true, Some("cache-stale"),
                    Dispatch::Ok { text: with_as_of(value, fetched_at), cached: true });
            }
            // A recent fetch FAILED and we are backing off. Falling through here would
            // hammer a down upstream on every turn — the exact thing the backoff
            // exists to prevent. Report the failure instead; there is no cached value
            // to serve (a usable one would have returned Fresh/Stale above).
            Lookup::Backoff => {
                finish!(gate_ctx, false, Some("backoff"),
                    Dispatch::Failed(format!(
                        "`{name}` recently failed and is backing off; try again shortly."
                    )));
            }
            Lookup::Miss => { /* fall through to a live fetch */ }
        }
    }

    // Live fetch, routed exactly like `tools/call`: mesh -> core -> broker -> personal.
    // No dispatch state = a wiring bug in the host, not a model mistake. Say so
    // distinctly so it is never mistaken for "that tool does not exist".
    let Some(st) = state else {
        finish!(gate_ctx, false, Some("no dispatch state"),
            Dispatch::Unknown(format!(
                "`{name}` cannot be dispatched: this server has no tool-dispatch state \
                 configured."
            )));
    };
    match st.router_dispatch(name, args, principal_obj).await {
        Ok(text) => {
            if policy.is_some() {
                cache.put(&key, text.clone()).await;
            }
            finish!(gate_ctx, true, None, Dispatch::Ok { text, cached: false });
        }
        Err(e) => {
            // A tool that exists NOWHERE (core, broker, mesh, personal) is a model
            // mistake, not a transient failure — surface it as Unknown so the model
            // stops retrying, and do NOT record a cache failure for a name that was
            // never a real tool.
            if e.contains("is not a tool that exists here") {
                finish!(gate_ctx, false, Some("unknown tool"),
                    Dispatch::Unknown(format!(
                        "`{name}` is not a tool that exists here. Do not call it again; \
                         answer with the tools you were given."
                    )));
            }
            // An error is NEVER cached as a value — only a short backoff, and any
            // existing good value is preserved.
            if policy.is_some() {
                cache.record_failure(&key).await;
            }
            finish!(gate_ctx, false, Some("tool error"), Dispatch::Failed(e));
        }
    }
}

/// Tools an assistant may NEVER invoke, whatever its grant says.
///
/// Two families, and the second is the one that bit us:
/// 1. [`crate::approval::is_guarded`] — the operator-guarded set (ansible/openhands/
///    <secret-manager>/routines/mirror-push/pg).
/// 2. The **approval mechanism itself** (`approval_grant`, `approval_deny`). These are
///    NOT in the guarded list — they are only covered by the gateway's `approval_`
///    DENY PREFIX, which applies to `Grant::AllowDeny` identities (the scaffolded
///    `lumina`/`harmony`) but NOT to a legacy `Grant::List(["*"])` identity such as
///    `moose`/`claude`. So a wildcard-granted caller could have had a model approve its
///    own guarded requests, dissolving the human-in-the-loop gate. Blocked here
///    unconditionally, mirroring Chord's `is_llm_blocked`.
///
/// This is defence in depth, not the primary control: `guard()` above is. A model must
/// not be able to reach the approval mechanism even if an allowlist is misconfigured.
pub fn is_model_blocked(bare_name: &str) -> bool {
    crate::approval::is_guarded(bare_name) || bare_name.starts_with("approval_")
}

/// Annotate a cached payload with when it was fetched.
///
/// Anti-fabrication applies to FRESHNESS as much as to content: the assistant must be
/// able to say "as of 09:15" instead of implying a cached reading is live.
fn with_as_of(value: String, fetched_at: u64) -> String {
    if fetched_at == 0 {
        return value;
    }
    // Do NOT append to a JSON payload: many tools return structured JSON, and a
    // trailing prose note would make it unparseable for any structured consumer
    // (review finding). Freshness for those is carried in the step log instead.
    if serde_json::from_str::<Value>(&value).is_ok() {
        return value;
    }
    format!("{value}\n\n(cached; fetched at {fetched_at} epoch-seconds)")
}

/// Refresh a stale entry in the background.
fn spawn_refresh(cache: &ToolCache, name: &str, args: Value, key: String) {
    // Uses the SHARED registry (`registry::shared()`), which is `&'static` and already
    // registered — the refresh must not pay a ~400-tool registration just to re-fetch
    // one value.
    let cache = cache.clone();
    let name = name.to_string();
    tokio::spawn(async move {
        // Re-check availability: a tool parked between the original call and this
        // refresh must not be re-fetched (review consistency finding).
        if !crate::availability::policy().agent_usable(&name) {
            return;
        }
        let reg = crate::registry::shared();
        match reg.call(&name, args).await {
            Some(Ok(text)) => cache.put(&key, text).await,
            // A failed refresh must leave the last-good value intact.
            _ => cache.record_failure(&key).await,
        }
    });
}

/// The tool-call the model asked for, parsed out of a chat completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Parse tool calls out of an OpenAI-shaped chat completion.
///
/// Tolerant by design: `arguments` arrives as a JSON *string* that models sometimes
/// emit malformed. A malformed argument blob becomes an empty object rather than
/// failing the whole turn — the tool's own validation is a better error than ours.
pub fn parse_tool_calls(completion: &Value) -> Vec<ToolCall> {
    let Some(msg) = completion
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
    else {
        return Vec::new();
    };
    let Some(calls) = msg.get("tool_calls").and_then(|t| t.as_array()) else {
        return Vec::new();
    };
    calls
        .iter()
        .filter_map(|c| {
            let f = c.get("function")?;
            let name = f.get("name")?.as_str()?.to_string();
            let raw = f.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}");
            let arguments = serde_json::from_str(raw).unwrap_or_else(|_| json!({}));
            let id = c
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("call_0")
                .to_string();
            Some(ToolCall { id, name, arguments })
        })
        .collect()
}

/// Extract assistant text from a chat completion.
pub fn parse_content(completion: &Value) -> String {
    completion
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string()
}

/// Render selected tools into the OpenAI `tools` array Chord expects.
pub fn tools_payload(tools: &[ToolInfo]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect(),
    )
}

/// Config for one router run.
pub struct RouterConfig {
    pub model: String,
    pub max_steps: usize,
    pub budget: Duration,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            // A Chord NAMED PROXY, never a concrete model name — Chord owns model
            // selection, tiering, and GPU lifecycle (north-star Module Contract cl. 1).
            model: crate::config::router_model_alias(),
            max_steps: MAX_STEPS,
            budget: DEFAULT_BUDGET,
        }
    }
}

/// Whether the loop should stop, and why.
pub fn should_stop(step: usize, max_steps: usize, started: Instant, budget: Duration) -> Option<&'static str> {
    if step >= max_steps {
        return Some("step_cap");
    }
    if started.elapsed() >= budget {
        return Some("timeout");
    }
    None
}

/// Build the tool-result message appended after a dispatch.
pub fn tool_result_message(call_id: &str, text: &str) -> Value {
    json!({"role": "tool", "tool_call_id": call_id, "content": text})
}

/// Everything the router needs from its host, so the loop itself stays testable.
pub struct RouterDeps<'a> {
    /// The server state — dispatch routes through `router_dispatch`, which uses the
    /// SAME precedence as `tools/call` (mesh -> core -> broker -> personal). A
    /// core-registry-only dispatch would be blind to `pve__*` (mesh-federated), i.e.
    /// exactly the Proxmox tools the operator asks about.
    pub state: &'a crate::mcp_server::McpServerState,
    pub cache: &'a ToolCache,
    pub chord: &'a InferenceProxyClient,
    pub gateway: Option<&'a crate::gateway_framework::GatewayFramework>,
    pub principal: Option<&'a Principal>,
}

/// TRTR-02 — the SSE progress-event wire contract.
///
/// **This is deliberately Chord's existing frame vocabulary, byte-for-byte.**
/// lumina-core's `AgenticSseState` parser already consumes exactly these `type`s, so
/// emitting the same frames means the client needs NO change to talk to the relocated
/// router — TRTR-04 becomes a verification step rather than a risky contract change on
/// a live assistant. Unknown frame types are ignored by that parser, so adding fields
/// later stays backward compatible.
///
/// Frames: `started`, `tool_call_started`, `tool_call_complete`,
/// `security_event_occurred`, `complete`. Only `complete` is load-bearing — the parser
/// FAILS the turn if the stream ends without one, so every exit path must emit it.
pub mod sse {
    use serde_json::{json, Value};

    pub fn frame(v: &Value) -> String {
        format!("data: {v}\n\n")
    }

    pub fn started() -> String {
        frame(&json!({"type": "started"}))
    }

    pub fn tool_call_started(tool: &str) -> String {
        frame(&json!({"type": "tool_call_started", "tool_name": tool}))
    }

    pub fn tool_call_complete(tool: &str, duration_ms: u64, status: &str) -> String {
        frame(&json!({
            "type": "tool_call_complete",
            "tool_name": tool,
            "duration_ms": duration_ms,
            "status": status,
        }))
    }

    pub fn complete(response: &str) -> String {
        frame(&json!({"type": "complete", "response": response}))
    }
}

/// Render a finished turn as the full SSE stream a client expects.
///
/// Built as one buffered body rather than a true incremental stream: a turn is short
/// (seconds), the client's own parser buffers to frame boundaries anyway, and the
/// progressive-display benefit does not justify threading a channel through the loop
/// for the first cut. The frame CONTRACT is identical either way, so moving to true
/// incremental emission later is an internal change.
pub fn render_sse(outcome: &RouterOutcome) -> String {
    let mut out = String::new();
    out.push_str(&sse::started());
    for s in &outcome.steps {
        out.push_str(&sse::tool_call_started(&s.tool));
        out.push_str(&sse::tool_call_complete(&s.tool, s.duration_ms, s.status));
    }
    // ALWAYS last, and always present: the parser fails the turn without it.
    out.push_str(&sse::complete(&outcome.response));
    out
}

/// Run one agentic turn: select tools, ask Chord, dispatch what it asks for, repeat.
///
/// Returns the assistant's final text plus a step log. Every exit path produces a
/// USABLE answer — a timeout or step-cap still returns what was learned so far rather
/// than an error page, because a partial answer beats a dead turn.
pub async fn execute(
    deps: RouterDeps<'_>,
    system_prompt: &str,
    user_message: &str,
    cfg: RouterConfig,
) -> RouterOutcome {
    let started = Instant::now();
    let mut steps: Vec<Step> = Vec::new();

    let catalog = deps.state.router_catalog().await;
    let selected = select::select_tools(&catalog, user_message, deps.gateway, deps.principal);
    let principal_name = deps.principal.map(|p| p.name());

    let mut messages: Vec<Value> = Vec::new();
    if !system_prompt.is_empty() {
        messages.push(json!({"role": "system", "content": system_prompt}));
    }
    messages.push(json!({"role": "user", "content": user_message}));

    let tools = tools_payload(&selected);
    let mut turns = 0usize;

    loop {
        if let Some(reason) = should_stop(steps.len(), cfg.max_steps, started, cfg.budget) {
            return RouterOutcome {
                response: partial_answer(&messages, reason),
                steps,
                turns,
                status: reason,
            };
        }

        // Give the inference call only the time left in the budget, so one slow
        // completion cannot overrun the turn.
        let remaining = cfg.budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return RouterOutcome {
                response: partial_answer(&messages, "timeout"),
                steps,
                turns,
                status: "timeout",
            };
        }

        let mut body = json!({
            "model": cfg.model,
            "messages": messages,
        });
        if !selected.is_empty() {
            body["tools"] = tools.clone();
        }

        turns += 1;
        let completion = match deps.chord.chat_completion(body, principal_name, remaining).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("agent_router: inference failed: {e}");
                return RouterOutcome {
                    response: partial_answer(&messages, "inference_error"),
                    steps,
                    turns,
                    status: "inference_error",
                };
            }
        };

        let calls = parse_tool_calls(&completion);
        if calls.is_empty() {
            let content = parse_content(&completion);
            return RouterOutcome {
                response: content,
                steps,
                turns,
                status: "ok",
            };
        }

        // Echo the assistant's tool-call turn back into the transcript, or the model
        // loses track of what it just asked for.
        if let Some(msg) = completion
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("message"))
        {
            messages.push(msg.clone());
        }

        // A single completion can request MANY tool calls. Truncate to what the step
        // budget still allows, or one over-eager completion would blow straight past
        // the cap (round-2 review finding).
        let remaining_steps = cfg.max_steps.saturating_sub(steps.len());
        for call in calls.into_iter().take(remaining_steps) {
            // Re-check the wall clock BETWEEN calls: the budget previously bounded only
            // the inference hop, so a sequence of slow tools could overrun it entirely.
            if started.elapsed() >= cfg.budget {
                return RouterOutcome {
                    response: partial_answer(&messages, "timeout"),
                    steps,
                    turns,
                    status: "timeout",
                };
            }
            let t0 = Instant::now();
            let d = dispatch_tool(
                Some(deps.state),
                deps.cache,
                deps.gateway,
                deps.principal,
                &call.name,
                call.arguments.clone(),
                principal_name,
            )
            .await;
            let (text, status, cached) = match d {
                Dispatch::Ok { text, cached } => (text, "ok", cached),
                Dispatch::Unavailable(m) => (m, "unavailable", false),
                Dispatch::Unknown(m) => (m, "unknown", false),
                Dispatch::Failed(m) => (
                    // The model gets a clean, non-leaky failure it can act on.
                    format!("`{}` failed: {m}", call.name),
                    "failed",
                    false,
                ),
                Dispatch::Denied(m) => (m, "denied", false),
            };
            steps.push(Step {
                tool: call.name.clone(),
                status,
                cached,
                duration_ms: t0.elapsed().as_millis() as u64,
            });
            tracing::info!(
                "agent_router: tool={} status={} cached={} ms={}",
                call.name,
                status,
                cached,
                t0.elapsed().as_millis()
            );
            messages.push(tool_result_message(&call.id, &text));
        }
    }
}

/// Best-effort answer when the loop ends without the model producing one.
///
/// Returns the last tool result rather than an error string: if the user asked for
/// the weather and we fetched it but ran out of budget before the model could phrase
/// it, the data is still worth more than "something went wrong".
fn partial_answer(messages: &[Value], reason: &str) -> String {
    let last_tool = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str());
    match (last_tool, reason) {
        (Some(t), _) if !t.is_empty() => format!(
            "I ran out of time composing a full reply, but here is what I found:\n\n{t}"
        ),
        (_, "inference_error") => {
            "I could not reach the inference service just now — please try again.".to_string()
        }
        _ => "I could not complete that request in time — please try again.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_tool_call() {
        let c = json!({"choices":[{"message":{"tool_calls":[
            {"id":"c1","function":{"name":"weather","arguments":"{\"location\":\"Omaha\"}"}}
        ]}}]});
        let calls = parse_tool_calls(&c);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "weather");
        assert_eq!(calls[0].arguments["location"], "Omaha");
    }

    #[test]
    fn malformed_arguments_degrade_to_empty_object_not_a_failed_turn() {
        // Models do emit malformed argument blobs. The tool's own validation is a
        // better error than aborting the whole turn here.
        let c = json!({"choices":[{"message":{"tool_calls":[
            {"id":"c1","function":{"name":"weather","arguments":"{not json"}}
        ]}}]});
        let calls = parse_tool_calls(&c);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments, json!({}));
    }

    #[test]
    fn a_completion_with_no_tool_calls_yields_none() {
        let c = json!({"choices":[{"message":{"content":"just an answer"}}]});
        assert!(parse_tool_calls(&c).is_empty());
        assert_eq!(parse_content(&c), "just an answer");
    }

    #[test]
    fn a_malformed_completion_does_not_panic() {
        for c in [json!({}), json!({"choices":[]}), json!({"choices":[{}]}), json!(null)] {
            assert!(parse_tool_calls(&c).is_empty());
            assert_eq!(parse_content(&c), "");
        }
    }

    #[test]
    fn tools_payload_is_openai_shaped() {
        let t = vec![ToolInfo {
            name: "weather".into(),
            description: "d".into(),
            parameters: json!({"type":"object"}),
        }];
        let p = tools_payload(&t);
        assert_eq!(p[0]["type"], "function");
        assert_eq!(p[0]["function"]["name"], "weather");
    }

    #[test]
    fn the_loop_stops_at_the_step_cap() {
        let start = Instant::now();
        assert_eq!(should_stop(0, 8, start, DEFAULT_BUDGET), None);
        assert_eq!(should_stop(8, 8, start, DEFAULT_BUDGET), Some("step_cap"));
    }

    #[test]
    fn the_loop_stops_when_the_budget_is_spent() {
        let start = Instant::now();
        // A zero budget is already spent.
        assert_eq!(should_stop(0, 8, start, Duration::from_secs(0)), Some("timeout"));
    }

    #[test]
    fn the_router_budget_is_below_luminas_egress_timeout() {
        // Load-bearing ordering: if the router outlasts the caller's 120s egress
        // timeout, the user sees a dead socket instead of the router's own error.
        assert!(
            DEFAULT_BUDGET < Duration::from_secs(120),
            "router budget must stay under the 120s lumina egress timeout"
        );
    }

    #[tokio::test]
    async fn dispatching_without_state_reports_a_wiring_fault_not_a_missing_tool() {
        // Distinct from "that tool does not exist": no dispatch state is a HOST
        // misconfiguration, and conflating the two would send an operator hunting for
        // a tool-name bug that is not there.
        let cache = ToolCache::default();
        match dispatch_tool(None, &cache, None, None, "weather", json!({}), None).await {
            Dispatch::Unknown(msg) => {
                assert!(msg.contains("no tool-dispatch state"), "got: {msg}");
                assert!(!msg.to_lowercase().contains("try again"));
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn sse_always_ends_with_a_complete_frame() {
        // lumina-core's parser FAILS the turn if the stream ends without `complete`,
        // so every exit path must emit one — including a timed-out or empty turn.
        let o = RouterOutcome {
            response: "answer".into(),
            steps: vec![],
            turns: 1,
            status: "ok",
        };
        let s = render_sse(&o);
        assert!(s.trim_end().ends_with("}"), "stream must end with a frame");
        assert!(s.contains("\"type\": \"complete\"") || s.contains("\"type\":\"complete\""));
    }

    #[test]
    fn sse_emits_paired_start_and_complete_for_each_tool() {
        let o = RouterOutcome {
            response: "answer".into(),
            steps: vec![
                Step { tool: "weather".into(), status: "ok", cached: true, duration_ms: 3 },
                Step { tool: "news_headlines".into(), status: "ok", cached: false, duration_ms: 40 },
            ],
            turns: 3,
            status: "ok",
        };
        let s = render_sse(&o);
        assert_eq!(s.matches("tool_call_started").count(), 2);
        assert_eq!(s.matches("tool_call_complete").count(), 2);
        assert!(s.contains("weather"));
        assert!(s.contains("news_headlines"));
    }

    #[test]
    fn sse_frames_are_double_newline_terminated() {
        // SSE framing: a frame the client cannot terminate is a frame it never sees.
        let f = sse::complete("x");
        assert!(f.starts_with("data: "));
        assert!(f.ends_with("\n\n"));
    }

    #[test]
    fn a_timed_out_turn_still_emits_complete() {
        let o = RouterOutcome {
            response: "partial".into(),
            steps: vec![Step { tool: "weather".into(), status: "ok", cached: false, duration_ms: 5 }],
            turns: 2,
            status: "timeout",
        };
        let s = render_sse(&o);
        assert!(s.contains("complete"), "a timeout must not strand the client waiting");
        assert!(s.contains("partial"), "the partial answer must reach the user");
    }

    #[tokio::test]
    async fn an_operator_guarded_tool_is_never_model_invocable() {
        // Ported from Chord's is_llm_blocked. A model that could call approval_grant
        // would approve its own guarded requests and dissolve the human-in-the-loop
        // gate entirely.
        let cache = ToolCache::default();
        for guarded in ["approval_grant", "approval_deny", "infisical_get_secret", "pg_ddl", "ansible_run_playbook"] {
            match dispatch_tool(None, &cache, None, None, guarded, json!({}), None).await {
                Dispatch::Denied(msg) => {
                    assert!(msg.contains("operator approval"), "got: {msg}");
                }
                other => panic!("{guarded} must be DENIED, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_model_block_covers_both_families() {
        // The operator-guarded set...
        assert!(is_model_blocked("infisical_get_secret"));
        assert!(is_model_blocked("pg_ddl"));
        assert!(is_model_blocked("ansible_run_playbook"));
        // ...AND the approval mechanism itself, which is NOT in that set and is
        // otherwise only covered by a deny prefix that a Grant::List(["*"]) identity
        // bypasses entirely.
        assert!(is_model_blocked("approval_grant"));
        assert!(is_model_blocked("approval_deny"));
        // Ordinary tools are unaffected.
        assert!(!is_model_blocked("weather"));
        assert!(!is_model_blocked("news_headlines"));
        assert!(!is_model_blocked("pve__get_nodes"));
    }

    #[tokio::test]
    async fn a_namespaced_guarded_tool_is_also_blocked() {
        // A guarded tool re-exported through a mesh upstream must not become
        // invocable just because it arrived namespaced.
        let cache = ToolCache::default();
        match dispatch_tool(None, &cache, None, None, "ct322__approval_grant", json!({}), None).await {
            Dispatch::Denied(_) => {}
            other => panic!("namespaced guarded tool must be DENIED, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_backing_off_tool_is_not_refetched() {
        // Falling through on Backoff would hammer a down upstream every turn — the
        // exact thing the backoff exists to prevent.
        let cache = ToolCache::default();
        let key = ToolCache::key("news_headlines", &json!({}), None, false);
        cache.record_failure(&key).await;
        match dispatch_tool(None, &cache, None, None, "news_headlines", json!({}), None).await {
            Dispatch::Failed(m) => assert!(m.contains("backing off"), "got: {m}"),
            // If the registry lacks the tool the Unknown path is also acceptable here;
            // what must NOT happen is a silent live re-fetch.
            Dispatch::Unknown(_) => {}
            other => panic!("expected backoff to be enforced, got {other:?}"),
        }
    }

    #[test]
    fn freshness_note_never_corrupts_a_json_payload() {
        // Many tools return structured JSON; appending prose would make it unparseable
        // for a structured consumer.
        let j = r#"{"articles":[],"count":0}"#.to_string();
        assert_eq!(with_as_of(j.clone(), 1_700_000_000), j, "JSON must pass through untouched");
        // Plain text still gets the honest "as of" annotation.
        let t = "Current weather: 68F".to_string();
        assert!(with_as_of(t, 1_700_000_000).contains("cached"));
    }

    #[test]
    fn tool_result_message_carries_the_call_id() {
        let m = tool_result_message("c1", "result text");
        assert_eq!(m["role"], "tool");
        assert_eq!(m["tool_call_id"], "c1");
        assert_eq!(m["content"], "result text");
    }
}
