//! Deprecation-alias tools for the 7 pure graph-relay Cortex tools retired in
//! CXEG-01 (`cortex_stats`, `cortex_build`, `cortex_deps`, `cortex_recent`,
//! `cortex_community`, `cortex_architecture`, `cortex_flows`).
//!
//! Each of these tool NAMES still exists in the registry (so a caller that
//! lists tools or calls one of these names by muscle memory doesn't just get
//! "tool not found"), but its `execute` body does no I/O of any kind — no
//! network, no SSH, no filesystem, no database. It only returns a structured
//! JSON pointer, `{"deprecated": true, "use": "<replacement tool name>", ...}`,
//! naming the Atlas KG (`crate::scribe::graph`) tool that replaces it.
//!
//! Replacement map (see `src/cortex/mod.rs`'s module doc for the full CXEG-01
//! rationale):
//!
//! | Retired tool          | Replacement       |
//! | ---------------------- | ------------------ |
//! | `cortex_stats`         | `kg_stats`          |
//! | `cortex_build`         | `scribe_kg_build`   |
//! | `cortex_deps`          | `kg_neighbors`      |
//! | `cortex_recent`        | `kg_query`          |
//! | `cortex_community`     | `kg_communities`    |
//! | `cortex_architecture`  | `kg_communities`    |
//! | `cortex_flows`         | `kg_path`           |

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

/// One retired-tool-name -> replacement-tool-name deprecation alias. `execute`
/// always succeeds (no I/O) and returns the structured pointer as pretty JSON.
struct DeprecatedAlias {
    name: &'static str,
    replacement: &'static str,
    note: &'static str,
}

#[async_trait]
impl RustTool for DeprecatedAlias {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.note
    }

    fn parameters(&self) -> Value {
        // Deliberately permissive: this tool never inspects its arguments
        // (it does nothing with them), so any shape a caller passes is
        // accepted without validation error, and the pointer is always
        // returned the same way regardless of input.
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": true
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let response = json!({
            "deprecated": true,
            "use": self.replacement,
            "message": format!(
                "'{}' was retired in CXEG-01 along with the rest of Cortex's \
                 SSH-relay-era transport to the now-retired fleet host. Call \
                 '{}' against the in-process Atlas KG instead.",
                self.name, self.replacement
            ),
        });
        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

/// Register all 7 deprecation aliases.
pub fn register(registry: &mut ToolRegistry) {
    let aliases: [(&'static str, &'static str, &'static str); 7] = [
        (
            "cortex_stats",
            "kg_stats",
            "DEPRECATED (CXEG-01): retired SSH-relay tool. Use kg_stats \
             against the Atlas KG instead. Returns a structured pointer, \
             performs no I/O.",
        ),
        (
            "cortex_build",
            "scribe_kg_build",
            "DEPRECATED (CXEG-01): retired SSH-relay tool. Use \
             scribe_kg_build to (re)build the Atlas KG instead. Returns a \
             structured pointer, performs no I/O.",
        ),
        (
            "cortex_deps",
            "kg_neighbors",
            "DEPRECATED (CXEG-01): retired SSH-relay tool. Use kg_neighbors \
             against the Atlas KG instead. Returns a structured pointer, \
             performs no I/O.",
        ),
        (
            "cortex_recent",
            "kg_query",
            "DEPRECATED (CXEG-01): retired SSH-relay tool. Use kg_query \
             against the Atlas KG instead. Returns a structured pointer, \
             performs no I/O.",
        ),
        (
            "cortex_community",
            "kg_communities",
            "DEPRECATED (CXEG-01): retired SSH-relay tool. Use \
             kg_communities against the Atlas KG instead. Returns a \
             structured pointer, performs no I/O.",
        ),
        (
            "cortex_architecture",
            "kg_communities",
            "DEPRECATED (CXEG-01): retired SSH-relay tool. Use \
             kg_communities against the Atlas KG instead. Returns a \
             structured pointer, performs no I/O.",
        ),
        (
            "cortex_flows",
            "kg_path",
            "DEPRECATED (CXEG-01): retired SSH-relay tool. Use kg_path \
             against the Atlas KG instead. Returns a structured pointer, \
             performs no I/O.",
        ),
    ];

    for (name, replacement, note) in aliases {
        let _ = registry.register(Box::new(DeprecatedAlias { name, replacement, note }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED: &[(&str, &str)] = &[
        ("cortex_stats", "kg_stats"),
        ("cortex_build", "scribe_kg_build"),
        ("cortex_deps", "kg_neighbors"),
        ("cortex_recent", "kg_query"),
        ("cortex_community", "kg_communities"),
        ("cortex_architecture", "kg_communities"),
        ("cortex_flows", "kg_path"),
    ];

    #[test]
    fn test_register_adds_all_seven_aliases() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        assert_eq!(registry.len(), 7);
        for (name, _) in EXPECTED {
            assert!(registry.contains(name), "missing alias {name}");
        }
    }

    #[tokio::test]
    async fn test_each_alias_returns_structured_deprecation_pointer() {
        for (name, replacement) in EXPECTED {
            let alias = DeprecatedAlias {
                name,
                replacement,
                note: "test",
            };
            let out = alias
                .execute(json!({"anything": "goes", "even": ["weird", "shapes"]}))
                .await
                .expect("deprecation alias must never error");
            let v: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(v["deprecated"], true, "tool {name}");
            assert_eq!(v["use"], *replacement, "tool {name}");
            assert!(v["message"].as_str().unwrap().contains(replacement), "tool {name}");
        }
    }

    #[tokio::test]
    async fn test_alias_execute_ignores_empty_args() {
        let alias = DeprecatedAlias {
            name: "cortex_stats",
            replacement: "kg_stats",
            note: "test",
        };
        let out = alias.execute(json!({})).await.expect("must not error on empty args");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["deprecated"], true);
        assert_eq!(v["use"], "kg_stats");
    }

    #[test]
    fn test_all_alias_names_start_with_cortex() {
        for (name, _) in EXPECTED {
            assert!(name.starts_with("cortex_"), "{name}");
        }
    }
}
