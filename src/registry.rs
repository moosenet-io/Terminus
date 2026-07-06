//! Tool registry: discovers and dispatches Rust tool implementations.
//!
//! Each Rust tool module (plane, gitea, nexus, etc.) calls `register_all`
//! at startup to add its tools to the shared registry. The registry is then
//! passed to the chord-proxy TerminusAdapter for fallback dispatch.

use std::collections::HashMap;
use serde_json::Value;

use crate::error::ToolError;
use crate::tool::RustTool;

/// Registry of all compiled-in Rust tool implementations.
///
/// Tools are identified by name. On dispatch, the registry finds the matching
/// tool and calls its `execute` method. Duplicate names are rejected at
/// registration time (first registration wins and returns an error for duplicates).
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn RustTool>>,
    /// Ordered list for catalog output (preserves registration order)
    order: Vec<String>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            order: Vec::new(),
        }
    }

    /// Register a tool. Returns an error if the name is already taken.
    pub fn register(&mut self, tool: Box<dyn RustTool>) -> Result<(), String> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(format!("Tool '{name}' already registered"));
        }
        self.order.push(name.clone());
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Register, silently replacing any existing tool with the same name.
    pub fn register_or_replace(&mut self, tool: Box<dyn RustTool>) {
        let name = tool.name().to_string();
        if !self.tools.contains_key(&name) {
            self.order.push(name.clone());
        }
        self.tools.insert(name, tool);
    }

    /// Return all tools in registration order.
    pub fn list(&self) -> Vec<ToolInfo> {
        self.order
            .iter()
            .filter_map(|name| {
                self.tools.get(name).map(|t| ToolInfo {
                    name: t.name().to_string(),
                    description: t.description().to_string(),
                    parameters: t.parameters(),
                })
            })
            .collect()
    }

    /// Execute a named tool with the given arguments.
    pub async fn call(&self, name: &str, args: Value) -> Option<Result<String, ToolError>> {
        let tool = self.tools.get(name)?;
        Some(tool.execute(args).await)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Metadata for a registered tool (for catalog listing).
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Register all compiled-in Rust tools into the registry.
///
/// Each tool module provides its own `register` function. This top-level
/// function calls all of them in sequence. CHORD-06..13 populate this.
pub fn register_all(registry: &mut ToolRegistry) {
    crate::ansible::register(registry);
    crate::approval::register(registry);
    crate::council::register(registry);
    crate::crucible::register(registry);
    crate::dev::register(registry);
    crate::gateway::register(registry);
    crate::<secret-manager>::register(registry);
    crate::intake::register(registry);
    crate::lumina_ext::register(registry);
    crate::model_advisor::register(registry);
    crate::network::register(registry);
    crate::openhands::register(registry);
    crate::axon::register(registry);
    crate::commute::register(registry);
    crate::dgem::register(registry);
    crate::weather::register(registry);
    crate::dura::register(registry);
    crate::gitea::register(registry);
    crate::github::register(registry);
    crate::google::register(registry);
    crate::<media-service>::register(registry);
    crate::litellm::register(registry);
    crate::<container-mgr>::register(registry);
    crate::prometheus::register(registry);
    crate::hearth::register(registry);
    crate::ledger::register(registry);
    crate::myelin::register(registry);
    crate::news::register(registry);
    crate::nexus::register(registry);
    crate::plane::register(registry);
    crate::relay::register(registry);
    crate::reminder::register(registry);
    crate::routines::register(registry);
    crate::seer::register(registry);
    crate::sentinel::register(registry);
    crate::sysversion::register(registry);
    crate::vector::register(registry);
    crate::vigil::register(registry);
    crate::vitals::register(registry);
    crate::wizard::register(registry);
    crate::tools::register(registry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::RustTool;

    struct TestTool { name: &'static str, desc: &'static str }

    #[async_trait::async_trait]
    impl RustTool for TestTool {
        fn name(&self) -> &str { self.name }
        fn description(&self) -> &str { self.desc }
        fn parameters(&self) -> Value { serde_json::json!({}) }
        async fn execute(&self, args: Value) -> Result<String, ToolError> {
            Ok(format!("{}:{args}", self.name))
        }
    }

    #[test]
    fn test_register_single_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(TestTool { name: "tool_a", desc: "A tool" })).unwrap();
        assert_eq!(reg.len(), 1);
        assert!(reg.contains("tool_a"));
    }

    #[test]
    fn test_register_duplicate_returns_error() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(TestTool { name: "tool_a", desc: "first" })).unwrap();
        let result = reg.register(Box::new(TestTool { name: "tool_a", desc: "second" }));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already registered"));
    }

    #[test]
    fn test_register_or_replace_overwrites() {
        let mut reg = ToolRegistry::new();
        reg.register_or_replace(Box::new(TestTool { name: "tool_a", desc: "v1" }));
        reg.register_or_replace(Box::new(TestTool { name: "tool_a", desc: "v2" }));
        assert_eq!(reg.len(), 1);
        let info = reg.list();
        assert_eq!(info[0].description, "v2");
    }

    #[test]
    fn test_list_preserves_registration_order() {
        let mut reg = ToolRegistry::new();
        for name in &["c_tool", "a_tool", "b_tool"] {
            reg.register(Box::new(TestTool { name, desc: "x" })).unwrap();
        }
        let tool_list = reg.list();
        let names: Vec<&str> = tool_list.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["c_tool", "a_tool", "b_tool"]);
    }

    #[tokio::test]
    async fn test_call_found_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(TestTool { name: "echo", desc: "echo" })).unwrap();
        let result = reg.call("echo", serde_json::json!({"msg": "hi"})).await;
        assert!(result.is_some());
        let text = result.unwrap().unwrap();
        assert!(text.contains("echo"));
    }

    #[tokio::test]
    async fn test_call_not_found_returns_none() {
        let reg = ToolRegistry::new();
        let result = reg.call("missing", serde_json::json!({})).await;
        assert!(result.is_none());
    }

    #[test]
    fn test_is_empty_initially() {
        let reg = ToolRegistry::new();
        assert!(reg.is_empty());
    }

    #[test]
    fn test_is_not_empty_after_register() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(TestTool { name: "t", desc: "d" })).unwrap();
        assert!(!reg.is_empty());
    }
}
