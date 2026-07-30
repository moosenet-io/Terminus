//! TAVAIL-01 — `tool_availability`: the ADMIN view of the registry.
//!
//! The operator's requirement was explicitly NOT to delete dead tools:
//! *"it's helpful to see them in the registry, but they should be in an off position
//! not available to the agents."* The `tools/list` filter satisfies the second half;
//! THIS tool satisfies the first, by listing every registered tool together with its
//! availability state and the operator's reason.
//!
//! Without this the registry would be honest to the agent but opaque to the human —
//! a parked tool would simply vanish, which is the de-registration outcome the
//! operator rejected.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::availability::{policy, Availability};
use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

pub struct ToolAvailability;

impl ToolAvailability {
    const NAME: &'static str = "tool_availability";
}

#[async_trait]
impl RustTool for ToolAvailability {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Admin view of tool availability. Lists tool families and their state — \
         available / off / broken — with the operator's reason. Tools switched OFF \
         remain REGISTERED and visible here, but are hidden from agent tool listings \
         and refused at call time. Use this to see what has been parked and why. \
         Optional `prefix` filters to one family; `state` filters to one state."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prefix": {
                    "type": "string",
                    "description": "Only report tools whose name starts with this \
                                    prefix (e.g. 'crucible_')."
                },
                "state": {
                    "type": "string",
                    "enum": ["available", "off", "broken"],
                    "description": "Only report tools in this state."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let prefix = args.get("prefix").and_then(Value::as_str).unwrap_or("");
        let want = args.get("state").and_then(Value::as_str);

        let pol = policy();
        // Report over the full compiled-in registry so a parked tool still SHOWS UP
        // here even though tools/list hides it from agents.
        let mut registry = ToolRegistry::new();
        crate::registry::register_all(&mut registry);

        let mut rows: Vec<Value> = Vec::new();
        let (mut n_avail, mut n_off, mut n_broken) = (0usize, 0usize, 0usize);

        for t in registry.list() {
            if !prefix.is_empty() && !t.name.starts_with(prefix) {
                continue;
            }
            let (state, reason, last_verified) = pol.record_of(&t.name);
            match state {
                Availability::Available => n_avail += 1,
                Availability::Off => n_off += 1,
                Availability::Broken => n_broken += 1,
            }
            if let Some(w) = want {
                if w != state.as_str() {
                    continue;
                }
            }
            rows.push(json!({
                "name": t.name,
                "state": state.as_str(),
                "agent_visible": state.agent_usable(),
                "reason": reason,
                // When the operator last CONFIRMED this state. Availability is
                // operator-confirmed, never auto-inferred from a failed probe, so a
                // stale stamp is the signal that a parked tool deserves re-checking.
                "last_verified": last_verified,
            }));
        }

        rows.sort_by(|a, b| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")));

        Ok(serde_json::to_string_pretty(&json!({
            "summary": {
                "available": n_avail,
                "off": n_off,
                "broken": n_broken,
                "policy_rules": pol.rule_count(),
            },
            "note": "Tools in state 'off' or 'broken' remain REGISTERED and listed here, \
                     but are hidden from agent tool listings and refused at call time.",
            "tools": rows,
        }))
        .unwrap_or_else(|e| format!("{{\"error\":\"serialize failed: {e}\"}}")))
    }
}

pub fn register(registry: &mut ToolRegistry) {
    if let Err(e) = registry.register(Box::new(ToolAvailability)) {
        tracing::warn!("availability: failed to register tool_availability: {e}");
    }
}
