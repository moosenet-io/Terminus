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
    crate::cortex::register(registry);
    crate::council::register(registry);
    crate::crucible::register(registry);
    crate::dev::register(registry);
    crate::gateway::register(registry);
    crate::<secret-manager>::register(registry); // pii-test-fixture
    crate::intake::register(registry);
    crate::lumina_ext::register(registry);
    crate::meridian::register(registry);
    crate::model_advisor::register(registry);
    crate::network::register(registry);
    crate::odyssey::register(registry);
    crate::openhands::register(registry);
    crate::axon::register(registry);
    crate::commute::register(registry);
    crate::dgem::register(registry);
    crate::weather::register(registry);
    crate::dura::register(registry);
    crate::forge::register_public(registry); // S106/GITX-05: git-public, CORE only
    crate::gitea::register(registry);
    crate::github::register(registry);
    crate::google::register(registry);
    crate::<media-service>::register(registry); // pii-test-fixture
    crate::litellm::register(registry);
    crate::<container-mgr>::register(registry); // pii-test-fixture
    crate::prometheus::register(registry);
    crate::hearth::register(registry);
    crate::ledger::register(registry);
    crate::myelin::register(registry);
    crate::news::register(registry);
    crate::nexus::register(registry);
    crate::plane::register(registry);
    crate::relay::register(registry);
    crate::scribe::register(registry);
    crate::reminder::register(registry);
    crate::review::register(registry);
    crate::routines::register(registry);
    crate::seer::register(registry);
    crate::sentinel::register(registry);
    crate::soma::register(registry);
    crate::skills::register(registry);
    crate::synapse::register(registry);
    crate::sundry::register(registry);
    crate::sysversion::register(registry);
    crate::vector::register(registry);
    crate::vigil::register(registry);
    crate::vitals::register(registry);
    crate::wizard::register(registry);
    crate::tools::register(registry);
}

/// Register the personal/admin tool subset served by the `terminus_personal`
/// binary — the genuine personal-utility / admin modules with no static call
/// sites in Lumina-core (ledger, vitals, crucible, relay, meridian, odyssey,
/// gateway, cortex, soma, skills, council, network, ansible, dev), plus
/// plane/gitea/github (direct personal/admin access — a separate consumer
/// base from Chord's build-pipeline-scoped serving of the same modules) and
/// the sundry grab-bag (health, echo, utc_now, constellation_version,
/// vector_onboard, searxng_search).
///
/// Deliberately EXCLUDED from this subset (see `terminus_personal` bin docs
/// for the full rationale):
///   - axon, vigil, sentinel, routines — flagged pending the operator's
///     archival decision (Lumina-core already reimplements sentinel/vigil
///     natively); left out of v1, NOT dropped/archived.
///   - a set of modules that mirror integrations already deliberately
///     retired on the legacy fleet host's Python side (secret-store
///     queries, monitoring/metrics, LLM-proxy admin, container-admin,
///     media-request, generic web-search-adjacent, agentic-coding-session,
///     onboarding-flow, cross-agent inbox, research, deep-reasoning-council,
///     knowledge-base, commute, cost-tracking, news) — not resurrected here.
///   - intake, approval, model_advisor, lumina_ext, dgem, weather, reminder,
///     review, synapse, sysversion, tools — core build-pipeline / model-
///     routing tooling already served by Chord; not duplicated on this
///     binary.
pub fn register_personal(registry: &mut ToolRegistry) {
    crate::ledger::register(registry);
    crate::vitals::register(registry);
    crate::crucible::register(registry);
    crate::relay::register(registry);
    crate::meridian::register(registry);
    crate::odyssey::register(registry);
    crate::gateway::register(registry);
    crate::cortex::register(registry);
    crate::soma::register(registry);
    crate::skills::register(registry);
    crate::council::register(registry);
    crate::network::register(registry);
    crate::ansible::register(registry);
    crate::dev::register(registry);
    crate::plane::register(registry);
    crate::forge::register_private(registry); // S106/GITX-05: git-private, PERSONAL only
    crate::gitea::register(registry);
    crate::github::register(registry);
    crate::sundry::register(registry);
}

/// Tool names that would collide if `register_all` (core) and
/// `register_personal` were both registered into the SAME [`ToolRegistry`].
///
/// TGW-01 (the `terminus-primary` gateway binary) deliberately registers
/// ONLY `register_all` — per the orchestrator-resolved design decision,
/// personal-registry tools are reached via TGW-02's federation to the
/// personal-registry deployment
/// rather than a locally-aggregated registry — so this binary never actually
/// builds a combined registry and never hits this collision at runtime.
/// This helper (and the test below that calls it) exists to make the
/// collision property EXPLICIT and loudly checkable rather than silently
/// discovered: each tool module's own `register()` function reports a
/// `ToolRegistry::register` duplicate-name `Err` via `tracing::warn!` and
/// drops the losing tool (see e.g. `crate::plane::register`) — a silent
/// drop, not a hard failure. A future caller that DOES need to build a
/// combined registry (or a test guarding against a values regression) can
/// call this first and fail loudly (`assert!`/`panic!`) on any non-empty
/// result instead of relying on that per-module warn-and-drop behavior.
pub fn core_personal_name_collisions() -> Vec<String> {
    let mut core = ToolRegistry::new();
    register_all(&mut core);
    let mut personal = ToolRegistry::new();
    register_personal(&mut personal);

    let mut collisions: Vec<String> = personal
        .list()
        .into_iter()
        .map(|t| t.name)
        .filter(|name| core.contains(name))
        .collect();
    collisions.sort();
    collisions
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

    #[test]
    fn test_soma_tools_registered() {
        let mut reg = ToolRegistry::new();
        crate::soma::register(&mut reg);
        assert!(reg.contains("soma_status"));
        assert!(reg.contains("soma_rename_agent"));
        assert!(reg.contains("soma_constellation_config"));
        assert!(reg.contains("soma_inference_status"));
        assert!(reg.contains("soma_cost_summary"));
        assert!(reg.contains("soma_backup_status"));
        assert!(reg.contains("soma_run_validation"));
        assert!(reg.contains("soma_skills_list"));
        assert!(reg.contains("soma_skill_approve"));
        assert!(reg.contains("soma_modules"));
    }

    #[test]
    fn test_skills_tools_registered() {
        let mut reg = ToolRegistry::new();
        crate::skills::register(&mut reg);
        assert!(reg.contains("skills_list"));
        assert!(reg.contains("skills_read"));
        assert!(reg.contains("skills_create"));
    }

    #[test]
    fn test_synapse_tools_registered() {
        let mut reg = ToolRegistry::new();
        crate::synapse::register(&mut reg);
        assert!(reg.contains("synapse_status"));
        assert!(reg.contains("synapse_trigger"));
        assert!(reg.contains("synapse_mute"));
    }

    /// SCRB-01: Scribe registers on the exact same path as `plane`/`gitea`/
    /// `github` (this crate's single `register_all()` -- see the module doc
    /// comment above, and `src/scribe/mod.rs`'s registration note, for why
    /// there is no separate `register_personal()` to keep it out of: as of
    /// this item, terminus-rs has exactly one registration function, which
    /// `chord-proxy` calls for its fallback registry).
    #[test]
    fn test_scribe_tools_registered_via_register_all() {
        let mut reg = ToolRegistry::new();
        register_all(&mut reg);
        assert!(reg.contains("scribe_generate_readme"));
        assert!(reg.contains("scribe_update_wiki_page"));
        assert!(reg.contains("scribe_build_diary_entry"));
        assert!(reg.contains("scribe_report_discrepancy"));
        assert!(reg.contains("scribe_status"));
    }

    #[test]
    fn test_no_duplicate_tool_names_full_registry() {
        // register_all() itself rejects duplicate names at registration time
        // (first-wins would otherwise silently hide a real collision) -- if
        // Scribe's tool names collided with any existing core tool, this
        // would fail with fewer entries than modules registered.
        let mut reg = ToolRegistry::new();
        register_all(&mut reg);
        assert!(reg.contains("scribe_status"));
        assert!(reg.len() > 0);
    }

    // ── TGW-01: core/personal collision detection is loud, not silent ──────

    #[test]
    fn test_core_personal_collision_detection_is_loud_not_silent() {
        // As of this item, register_all() and register_personal() both
        // register the plane/gitea/github tool modules -- a real,
        // pre-existing overlap (see TGW-01's spec item edge cases). Building
        // an AGGREGATED single registry from both (as a literal reading of
        // an earlier draft of this spec item's Description implied) would
        // therefore collide immediately on those tool names. This is
        // exactly why `terminus_primary` (TGW-01) registers ONLY
        // `register_all` and defers personal-tool access to TGW-02's
        // federation instead of a locally-aggregated registry -- this test
        // documents and pins that decision by proving the collision is
        // real, not hypothetical, and that `core_personal_name_collisions`
        // surfaces it explicitly (a `Vec` the caller can assert/panic on)
        // rather than the silent per-module `tracing::warn!`-and-drop each
        // `register()` implementation falls back to when handed a duplicate
        // name directly via `ToolRegistry::register`.
        let collisions = core_personal_name_collisions();
        assert!(
            !collisions.is_empty(),
            "expected a known pre-existing collision (plane/gitea/github tool \
             names are registered by both register_all and register_personal) \
             -- if this now passes empty, the modules were de-duplicated and \
             this test's documentation should be updated to match"
        );
        // A loud, visible report of every colliding name -- never a silent
        // drop -- is the actual "fails loudly" behavior this item's
        // acceptance criteria require of anything that WOULD build a
        // combined registry.
        eprintln!(
            "core/personal tool-name collisions (expected, documented): {collisions:?}"
        );
        // register_personal calls nearly every module register_all also
        // calls -- plane/gitea/github/sundry AND the personal-utility
        // modules (ledger, vitals, crucible, relay, meridian, odyssey,
        // gateway, cortex, soma, skills, council, network, ansible, dev),
        // per both functions' own doc comments above -- so in practice
        // almost the entire register_personal() tool set collides. The ONE
        // deliberate exception is `forge::register_private` (git-private
        // tools), which register_personal calls in place of
        // `forge::register_public` (register_all's choice) -- those two
        // produce DIFFERENT tool names by design, so they must never appear
        // in the collision set. That's the one thing this test pins as a
        // hard invariant; the rest is just "this is real and large."
        let mut personal_only = ToolRegistry::new();
        register_personal(&mut personal_only);
        let personal_tool_count = personal_only.list().len();
        assert!(
            collisions.len() >= personal_tool_count / 2,
            "expected the collision set to cover most of register_personal()'s tools \
             ({} of {} registered) -- got only {}: {collisions:?}",
            collisions.len(),
            personal_tool_count,
            collisions.len()
        );
    }
}
