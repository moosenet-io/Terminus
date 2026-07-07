//! Per-occurrence human-approval gate for guarded tools (openhands, <secret-manager>). // pii-test-fixture
//!
//! A guarded tool calls [`gate`] at the very start of its `execute()`:
//!   - If the args carry a valid `_approval_code` that is APPROVED, unexpired, and
//!     not yet consumed, the code is consumed (single use) and the call proceeds.
//!   - Otherwise a fresh 6-char code + a `pending` row are created and the call is
//!     refused with an "APPROVAL REQUIRED" message. The operator approves out of
//!     band — `approve <CODE>` in chat, which lumina-core handles deterministically
//!     (NOT an LLM turn): it marks the row approved and re-dispatches the stored
//!     call with the code, so the tool consumes it and runs exactly once.
//!
//! Grants live in `tool_approvals` in the lumina_inbox Postgres (`DATABASE_URL`),
//! shared between the sweep-harness host (this crate) and the orchestrator container
//! (lumina-core). The LLM cannot forge
//! an approval: only a row it never wrote, flipped to `approved` by the operator's
//! out-of-band command, lets a call through.
//!
//! ## Content-binding
//! A code is scoped to `(tool_name, args)`, not just `tool_name`: consumption
//! requires the args presented now (with `_approval_code` stripped) to match,
//! as JSON, the args that were pending when the operator saw the summary and
//! approved it. Without this, a code approved for one call could be redeemed
//! against a *different* set of args for the same tool — e.g. a single-slot
//! staging file (as `routines_approve` uses) gets overwritten with a more
//! destructive proposal between approval and redemption, or any other guarded
//! tool is re-invoked with different arguments in that window — executing
//! something the operator never actually saw or approved. Flagged by an
//! adversarial review of the routines-tools port and fixed here so every
//! guarded tool gets the protection, not just routines.

use serde_json::Value;
use sqlx::PgPool;

use crate::error::ToolError;

/// The argument key carrying an approval code on a re-dispatched guarded call.
pub const APPROVAL_ARG: &str = "_approval_code";

/// Outcome of the approval gate.
pub enum Gate {
    /// Approved + consumed — the tool may execute.
    Granted,
    /// No/!valid code — caller must return this message as its result and NOT execute.
    Pending(String),
    /// A code was supplied but is invalid/expired/used — return as the result.
    Denied(String),
}

async fn pool() -> Result<PgPool, ToolError> {
    let url = std::env::var("DATABASE_URL").map_err(|_| {
        ToolError::NotConfigured("DATABASE_URL not set — approval gate requires Postgres".into())
    })?;
    PgPool::connect(&url)
        .await
        .map_err(|e| ToolError::Database(format!("approval DB connect: {e}")))
}

/// 6-char uppercase code from an unambiguous alphabet (no I/O/0/1).
fn gen_code(seed: &str, salt: u8) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = nanos
        ^ seed
            .bytes()
            .fold(1469598103934665603u128, |a, b| { // pii-test-fixture
                (a ^ b as u128).wrapping_mul(1099511628211) // pii-test-fixture
            })
        ^ (salt as u128).wrapping_mul(2654435761); // pii-test-fixture
    const CH: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut s = String::with_capacity(6);
    for _ in 0..6 {
        s.push(CH[(h % CH.len() as u128) as usize] as char);
        h /= CH.len() as u128;
    }
    s
}

/// Remove the `_approval_code` field (if any) from `args`, returning the
/// content that is actually bound to a grant — both when a pending row is
/// first written and when a code is later redeemed. Kept as one function so
/// the two call sites can never drift out of sync with each other.
fn content_of(args: &Value) -> Value {
    let mut stored = args.clone();
    if let Some(obj) = stored.as_object_mut() {
        obj.remove(APPROVAL_ARG);
    }
    stored
}

/// Gate a guarded tool call. See module docs.
pub async fn gate(tool_name: &str, args: &Value, summary: &str) -> Gate {
    let pool = match pool().await {
        Ok(p) => p,
        Err(e) => return Gate::Denied(format!("Approval system unavailable: {e}")),
    };

    let stored = content_of(args);

    if let Some(code) = args.get(APPROVAL_ARG).and_then(Value::as_str) {
        // Atomically consume an approved, unexpired, unused grant for this
        // exact tool AND this exact content — a code approved for one set of
        // args cannot be redeemed against a different set (see "Content-
        // binding" in the module docs).
        let consumed: Result<Option<String>, _> = sqlx::query_scalar(
            "UPDATE tool_approvals SET status = 'consumed', consumed_at = now() \
             WHERE code = $1 AND tool_name = $2 AND status = 'approved' \
               AND expires_at > now() AND consumed_at IS NULL \
               AND args_json = $3 \
             RETURNING code",
        )
        .bind(code)
        .bind(tool_name)
        .bind(&stored)
        .fetch_optional(&pool)
        .await;

        return match consumed {
            Ok(Some(_)) => Gate::Granted,
            Ok(None) => Gate::Denied(format!(
                "Approval code {code} is invalid, not yet approved, already used, expired, or \
                 was approved for different arguments than this call is making. \
                 Re-run the tool without a code to request a fresh approval."
            )),
            Err(e) => Gate::Denied(format!("Approval check failed: {e}")),
        };
    }

    // No code — create a pending request and tell the operator how to approve.
    for salt in 0..6u8 {
        let code = gen_code(&format!("{tool_name}|{summary}"), salt);
        let res = sqlx::query(
            "INSERT INTO tool_approvals (code, tool_name, args_json, args_summary) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&code)
        .bind(tool_name)
        .bind(&stored)
        .bind(summary)
        .execute(&pool)
        .await;
        if res.is_ok() {
            return Gate::Pending(format!(
                "⚠️ APPROVAL REQUIRED — `{tool_name}` is a guarded tool and was NOT run.\n\
                 Action: {summary}\n\
                 Reply `approve {code}` to authorize this single call (expires in 10 minutes), \
                 or `deny {code}` to reject."
            ));
        }
    }
    Gate::Denied("Could not create an approval request (repeated code collision).".into())
}

// ── Approval-management tools ─────────────────────────────────────────────────
//
// `approval_grant` / `approval_deny` flip a pending request. They are invoked
// ONLY by lumina-core's deterministic `approve <CODE>` / `deny <CODE>` command
// handler (a non-LLM path). chord-proxy HARD-BLOCKS both these and every guarded
// tool from being called inside the agentic loop, so the model can never approve
// its own request.

use async_trait::async_trait;
use serde_json::json;

use crate::registry::ToolRegistry;
use crate::tool::RustTool;

struct ApprovalGrant;
struct ApprovalDeny;

#[async_trait]
impl RustTool for ApprovalGrant {
    fn name(&self) -> &str { "approval_grant" }
    fn description(&self) -> &str {
        "INTERNAL: mark a pending guarded-tool approval as approved and return the \
stored call. Operator-only; never callable by the model."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object","properties":{"code":{"type":"string"}},"required":["code"]})
    }
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let code = args.get("code").and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("'code' required".into()))?;
        let pool = pool().await?;
        let row: Option<(String, serde_json::Value)> = sqlx::query_as(
            "UPDATE tool_approvals SET status='approved' \
             WHERE code=$1 AND status='pending' AND expires_at > now() \
             RETURNING tool_name, args_json",
        )
        .bind(code)
        .fetch_optional(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("grant failed: {e}")))?;
        match row {
            Some((tool_name, args_json)) => Ok(json!({
                "approved": true, "tool_name": tool_name, "args": args_json
            }).to_string()),
            None => Ok(json!({
                "approved": false,
                "error": format!("No pending approval for code {code} (already handled or expired).")
            }).to_string()),
        }
    }
}

#[async_trait]
impl RustTool for ApprovalDeny {
    fn name(&self) -> &str { "approval_deny" }
    fn description(&self) -> &str {
        "INTERNAL: reject a pending guarded-tool approval. Operator-only."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object","properties":{"code":{"type":"string"}},"required":["code"]})
    }
    async fn execute(&self, args: serde_json::Value) -> Result<String, ToolError> {
        let code = args.get("code").and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("'code' required".into()))?;
        let pool = pool().await?;
        let n = sqlx::query(
            "UPDATE tool_approvals SET status='denied' WHERE code=$1 AND status='pending'",
        )
        .bind(code)
        .execute(&pool)
        .await
        .map_err(|e| ToolError::Database(format!("deny failed: {e}")))?
        .rows_affected();
        Ok(json!({"denied": n > 0, "code": code}).to_string())
    }
}

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(ApprovalGrant));
    registry.register_or_replace(Box::new(ApprovalDeny));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use serde_json::json;

    #[test]
    fn gen_code_is_six_unambiguous_chars() {
        let c = gen_code("openhands_run|do X", 0);
        assert_eq!(c.len(), 6);
        assert!(c.chars().all(|ch| "ABCDEFGHJKLMNPQRSTUVWXYZ23456789".contains(ch)));
        // No ambiguous characters.
        assert!(!c.contains('I') && !c.contains('O') && !c.contains('0') && !c.contains('1'));
    }

    #[test]
    fn gen_code_varies_by_salt() {
        // Different salts should (almost always) give different codes.
        let a = gen_code("same", 0);
        let b = gen_code("same", 1);
        assert_ne!(a, b);
    }

    // ------------------------------------------------------------------
    // content_of — the content-binding fix. Same function computes what's
    // stored at proposal time and what's compared at redemption time, so a
    // grant for one set of args can never match a different set.
    // ------------------------------------------------------------------
    #[test]
    fn content_of_strips_approval_code_only() {
        let args = json!({ "name": "x", "_approval_code": "ABC123" });
        assert_eq!(content_of(&args), json!({ "name": "x" }));
    }

    #[test]
    fn content_of_different_args_are_not_equal() {
        let approved_for = content_of(&json!({ "name": "safe-thing", "_approval_code": "CODE01" }));
        let redeemed_with = content_of(&json!({ "name": "dangerous-thing", "_approval_code": "CODE01" }));
        assert_ne!(
            approved_for, redeemed_with,
            "content_of must distinguish different payloads so a code can't be replayed against them"
        );
    }

    #[test]
    fn content_of_identical_args_are_equal_regardless_of_code_value() {
        // Same real content, code stripped either way — this is what lets a
        // legitimate re-dispatch (same args, code attached) match the row
        // that was inserted (same args, no code).
        let a = content_of(&json!({ "name": "x", "n": 3 }));
        let b = content_of(&json!({ "name": "x", "n": 3, "_approval_code": "ANYCODE" }));
        assert_eq!(a, b);
    }

    #[tokio::test]
    #[serial]
    async fn gate_without_db_url_denies_gracefully() {
        std::env::remove_var("DATABASE_URL");
        match gate("openhands_run", &json!({"task": "x"}), "do x").await {
            Gate::Denied(m) => assert!(m.contains("unavailable") || m.contains("DATABASE_URL")),
            _ => panic!("expected Denied when DATABASE_URL unset"),
        }
    }

    #[test]
    fn approval_arg_constant() {
        assert_eq!(APPROVAL_ARG, "_approval_code");
    }
}
