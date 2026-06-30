//! CHORD-14: terminus-rs tool validation tests.
//!
//! Verifies that every tool module registers its expected tools into the
//! ToolRegistry. All tests are unit-level — no real backend required.
//!
//! Covers:
//!   - Per-module presence checks (one representative tool name per module)
//!   - Total tool count (>= 100)
//!   - No duplicate tool names across the full registry

use terminus_rs::{register_all, ToolRegistry};

// ── helpers ────────────────────────────────────────────────────────────────────

/// Build a fresh registry with every Rust tool registered.
fn full_registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    register_all(&mut reg);
    reg
}

// ── per-module presence tests ─────────────────────────────────────────────────

#[test]
fn test_plane_tools_registered() {
    let reg = full_registry();
    assert!(
        reg.contains("plane_list_projects"),
        "plane_list_projects must be registered"
    );
}

#[test]
fn test_gitea_tools_registered() {
    let reg = full_registry();
    assert!(
        reg.contains("gitea_list_repos"),
        "gitea_list_repos must be registered"
    );
}

#[test]
fn test_nexus_tools_registered() {
    let reg = full_registry();
    assert!(
        reg.contains("nexus_send"),
        "nexus_send must be registered"
    );
}

#[test]
fn test_axon_tools_registered() {
    let reg = full_registry();
    assert!(
        reg.contains("axon_submit"),
        "axon_submit must be registered"
    );
}

#[test]
fn test_hearth_tools_registered() {
    let reg = full_registry();
    assert!(
        reg.contains("hearth_pantry_list"),
        "hearth_pantry_list must be registered"
    );
}

#[test]
fn test_myelin_tools_registered() {
    let reg = full_registry();
    assert!(
        reg.contains("myelin_status"),
        "myelin_status must be registered"
    );
}

#[test]
fn test_dura_tools_registered() {
    let reg = full_registry();
    assert!(
        reg.contains("dura_smoke_test"),
        "dura_smoke_test must be registered"
    );
}

#[test]
fn test_vector_tools_registered() {
    let reg = full_registry();
    assert!(
        reg.contains("vector_submit"),
        "vector_submit must be registered"
    );
}

#[test]
fn test_seer_tools_registered() {
    let reg = full_registry();
    assert!(
        reg.contains("seer_query"),
        "seer_query must be registered"
    );
}

#[test]
fn test_relay_tools_registered() {
    let reg = full_registry();
    assert!(
        reg.contains("relay_vehicles"),
        "relay_vehicles must be registered"
    );
}

#[test]
fn test_vitals_tools_registered() {
    let reg = full_registry();
    assert!(
        reg.contains("vitals_today"),
        "vitals_today must be registered"
    );
}

#[test]
fn test_wizard_tools_registered() {
    let reg = full_registry();
    assert!(
        reg.contains("wizard_consult"),
        "wizard_consult must be registered"
    );
}

// ── complete module tool sets ─────────────────────────────────────────────────

#[test]
fn test_nexus_all_five_tools_registered() {
    let reg = full_registry();
    for tool in &["nexus_send", "nexus_check", "nexus_read", "nexus_ack", "nexus_history"] {
        assert!(reg.contains(tool), "Nexus tool '{tool}' must be registered");
    }
}

#[test]
fn test_axon_all_four_tools_registered() {
    let reg = full_registry();
    for tool in &["axon_submit", "axon_status", "axon_list", "axon_cancel"] {
        assert!(reg.contains(tool), "Axon tool '{tool}' must be registered");
    }
}

#[test]
fn test_plane_core_tools_registered() {
    let reg = full_registry();
    for tool in &[
        "plane_list_projects",
        "plane_get_project",
        "plane_list_work_items",
        "plane_get_work_item",
        "plane_create_work_item",
        "plane_update_work_item",
        "plane_delete_work_item",
    ] {
        assert!(reg.contains(tool), "Plane tool '{tool}' must be registered");
    }
}

#[test]
fn test_gitea_core_tools_registered() {
    let reg = full_registry();
    for tool in &[
        "gitea_list_repos",
        "gitea_get_repo",
        "gitea_create_file",
        "gitea_read_file",
        "gitea_update_file",
    ] {
        assert!(reg.contains(tool), "Gitea tool '{tool}' must be registered");
    }
}

#[test]
fn test_hearth_all_tools_registered() {
    let reg = full_registry();
    for tool in &[
        "hearth_pantry_list",
        "hearth_pantry_add",
        "hearth_meal_plan",
        "hearth_shopping_list",
        "hearth_what_can_i_make",
        "hearth_recipe_search",
        "hearth_stock_check",
    ] {
        assert!(reg.contains(tool), "Hearth tool '{tool}' must be registered");
    }
}

#[test]
fn test_myelin_all_tools_registered() {
    let reg = full_registry();
    for tool in &[
        "myelin_status",
        "myelin_today",
        "myelin_weekly",
        "myelin_monthly",
        "myelin_runaway_check",
        "myelin_burn_plan",
        "myelin_by_model",
        "myelin_by_user",
        "myelin_cap_check",
    ] {
        assert!(reg.contains(tool), "Myelin tool '{tool}' must be registered");
    }
}

#[test]
fn test_dura_all_tools_registered() {
    let reg = full_registry();
    for tool in &[
        "dura_smoke_test",
        "dura_backup_status",
        "dura_log_query",
        "dura_constellation_health",
        "dura_container_status",
        "dura_disk_usage",
        "dura_service_check",
    ] {
        assert!(reg.contains(tool), "Dura tool '{tool}' must be registered");
    }
}

#[test]
fn test_relay_all_tools_registered() {
    let reg = full_registry();
    for tool in &[
        "relay_vehicles",
        "relay_fuel_log",
        "relay_service_log",
        "relay_next_due",
        "relay_odometer",
    ] {
        assert!(reg.contains(tool), "Relay tool '{tool}' must be registered");
    }
}

#[test]
fn test_vitals_all_tools_registered() {
    let reg = full_registry();
    for tool in &[
        "vitals_log_weight",
        "vitals_today",
        "vitals_summary",
        "vitals_log_exercise",
        "vitals_log_sleep",
    ] {
        assert!(reg.contains(tool), "Vitals tool '{tool}' must be registered");
    }
}

#[test]
fn test_seer_all_tools_registered() {
    let reg = full_registry();
    for tool in &["seer_query", "seer_status", "seer_recent"] {
        assert!(reg.contains(tool), "Seer tool '{tool}' must be registered");
    }
}

#[test]
fn test_vector_core_tools_registered() {
    let reg = full_registry();
    for tool in &[
        "vector_submit",
        "vector_status",
        "vector_list",
        "vector_halt",
        "vector_resume",
    ] {
        assert!(reg.contains(tool), "Vector tool '{tool}' must be registered");
    }
}

// ── aggregate invariants ──────────────────────────────────────────────────────

/// The combined tool count across all modules must be at least 100.
#[test]
fn test_total_tool_count() {
    let reg = full_registry();
    let count = reg.len();
    assert!(
        count >= 100,
        "Expected at least 100 Rust tools registered, got {count}"
    );
}

/// No two tools may share the same name. Verifies the registry de-duplicates
/// correctly even when multiple modules register overlapping names.
#[test]
fn test_no_duplicate_tool_names() {
    let reg = full_registry();
    let info = reg.list();
    let mut names: Vec<&str> = info.iter().map(|t| t.name.as_str()).collect();
    let total = names.len();
    names.sort_unstable();
    names.dedup();
    let unique = names.len();
    assert_eq!(
        total, unique,
        "Duplicate tool names detected: {total} total, {unique} unique"
    );
}

/// Every registered tool must have a non-empty name, non-empty description,
/// and a JSON Schema object for its parameters.
#[test]
fn test_all_tools_have_valid_metadata() {
    let reg = full_registry();
    for info in reg.list() {
        assert!(!info.name.is_empty(), "Tool has empty name");
        assert!(
            !info.description.is_empty(),
            "Tool '{}' has empty description",
            info.name
        );
        assert!(
            info.parameters.is_object(),
            "Tool '{}' parameters must be a JSON object",
            info.name
        );
    }
}
