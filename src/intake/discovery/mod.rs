//! Model Discovery & Curation Agent (S114, the "Brochure").
//!
//! The brochure is a standing registry of HuggingFace model CANDIDATES for the
//! gfx1151 fleet — distinct from [`crate::intake::catalog`]'s Model Fleet
//! Catalog, which reports what has been TESTED. See `schema.rs`'s module doc for
//! the full naming-footgun explanation (this registry is always called the
//! "brochure," never "catalog").
//!
//! Submodules (added across S114 items):
//! - `schema` (DISC-01): the `model_discovery_candidate` table + its
//!   `FleetCategory`/`CandidateStatus` types (storage only).
//! - `hf_client` (DISC-04): the public HF Hub listing client that feeds
//!   discovery; a later DISC-05/DISC-06 classifies and persists its output.
//! - `storage` (DISC-02): the one Postgres read touch point
//!   (`storage::read_brochure`), reusing `crate::intake::storage::get_pool`.
//! - `tool` (DISC-02): the read-only `model_discovery_brochure` MCP core
//!   tool — a pure filter/render layer over `storage::read_brochure`'s
//!   output, registered on the core registry via [`register`] below.
//! - `upsert` (DISC-03): the one write API (`upsert_candidate`/
//!   `transition_status`/`record_eviction`) every other item uses to mutate
//!   brochure rows.

pub mod hf_client;
pub mod refresh;
pub mod schema;
pub mod storage;
pub mod tool;
pub mod upsert;

pub use schema::{CandidateStatus, DiscoveryCandidate, FleetCategory, Modality};

/// Register the brochure's MCP tools on the CORE registry. Wired into
/// `crate::intake::register` (the same Chord-served core surface
/// `catalog::register` uses) — never the personal registry.
///
/// DISC-02's read-only `model_discovery_brochure` (`tool`) plus DISC-06's
/// `model_discovery_refresh` curator (`refresh`) that populates it.
pub fn register(registry: &mut crate::registry::ToolRegistry) {
    tool::register(registry);
    refresh::register(registry);
}
