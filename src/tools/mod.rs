//! Terminus tool modules grouped under `tools/`.
//!
//! Most tool modules live at the crate root (one dir per integration). The
//! `tools/` namespace hosts the S85 serving control/status tools (SRV-07), which
//! sit ON TOP of the serving intake foundation (`crate::intake::serving`) and the
//! Chord control plane rather than a single external integration, and (as of
//! DOCGEN-01, S95) the `docgen` sovereign documentation-engine scaffold.

pub mod docgen;
pub mod serving_tools;

use crate::registry::ToolRegistry;

/// Register every tool under `tools/`.
pub fn register(registry: &mut ToolRegistry) {
    docgen::register(registry);
    serving_tools::register(registry);
}
