//! `pg_identities` — read-only listing of configured Postgres connection
//! identities and their privilege tiers (PGT-01).
//!
//! NEVER returns a secret value: only identity NAMES (from
//! `crate::pg::conn::configured_identities`, itself never exposing a URL) and
//! a name-derived privilege-tier label. Read-only and NOT guarded (see
//! `crate::approval::GUARDED_BARE_NAMES` — only the destructive `pg_*` tools
//! from later PGT items are guarded).

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

use super::conn;

pub struct PgIdentities;

#[async_trait]
impl RustTool for PgIdentities {
    fn name(&self) -> &str {
        "pg_identities"
    }

    fn description(&self) -> &str {
        "List the configured Postgres connection identities (from POSTGRES_URL_<NAME> \
         secrets) and their privilege tier (readonly/writer/admin/unknown), so a caller \
         can pick which `identity` to pass to other pg_* tools. Never returns a \
         connection URL or any other secret value -- names and tiers only."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, _args: Value) -> Result<ToolOutput, ToolError> {
        let names = conn::configured_identities();
        let identities: Vec<Value> = names
            .iter()
            .map(|name| {
                json!({
                    "identity": name,
                    "tier": conn::tier_for(name),
                })
            })
            .collect();

        let text = if names.is_empty() {
            "No Postgres connection identities are configured (no POSTGRES_URL_<NAME> \
             secrets present). Provision at least POSTGRES_URL_READONLY to enable the pg_* \
             tool suite."
                .to_string()
        } else {
            format!(
                "{} configured Postgres identit{}: {}",
                names.len(),
                if names.len() == 1 { "y" } else { "ies" },
                names
                    .iter()
                    .map(|n| format!("{n} ({})", conn::tier_for(n)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };

        Ok(ToolOutput::with_structured(
            text,
            json!({
                "identities": identities,
                "default_identity": conn::DEFAULT_IDENTITY,
            }),
        ))
    }
}

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(PgIdentities));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn clear_all() {
        for (k, _) in std::env::vars() {
            if k.starts_with("POSTGRES_URL_") {
                std::env::remove_var(k);
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn lists_configured_names_and_tiers_never_a_secret_value() {
        clear_all();
        std::env::set_var("POSTGRES_URL_READONLY", "postgres://ro-secret-value@example/db");
        std::env::set_var("POSTGRES_URL_ADMIN", "postgres://admin-secret-value@example/db");

        let out = PgIdentities.execute_structured(json!({})).await.unwrap();

        // The secret VALUE must never appear anywhere in the output.
        assert!(!out.text.contains("ro-secret-value"));
        assert!(!out.text.contains("admin-secret-value"));
        assert!(!out.text.contains("postgres://"));
        let structured = out.structured.expect("structured payload present");
        let dump = structured.to_string();
        assert!(!dump.contains("ro-secret-value"));
        assert!(!dump.contains("admin-secret-value"));
        assert!(!dump.contains("postgres://"));

        assert_eq!(structured["default_identity"], "readonly");
        let identities = structured["identities"].as_array().unwrap();
        assert_eq!(identities.len(), 2);
        clear_all();
    }

    #[tokio::test]
    #[serial]
    async fn empty_when_nothing_configured() {
        clear_all();
        let out = PgIdentities.execute_structured(json!({})).await.unwrap();
        let structured = out.structured.unwrap();
        assert_eq!(structured["identities"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn is_not_guarded() {
        assert!(!crate::approval::is_guarded("pg_identities"));
    }
}
