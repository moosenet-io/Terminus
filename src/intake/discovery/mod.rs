//! DISC-01 (S114, TERM #251): the discovery "brochure" module root.
//!
//! Storage-only for now (DISC-01's scope): the `model_discovery_candidate`
//! table (`schema.rs`) plus its `FleetCategory`/`CandidateStatus` types. The
//! brochure is a standing registry of HuggingFace model CANDIDATES for the
//! gfx1151 fleet — distinct from [`crate::intake::catalog`]'s Model Fleet
//! Catalog, which reports what has been TESTED. See `schema.rs`'s module doc
//! for the full naming-footgun explanation (this registry is always called the
//! "brochure," never "catalog").
//!
//! Later S114 items add to this module without changing DISC-01's shape:
//! DISC-02 adds `tool.rs` (the `model_discovery_brochure` MCP read tool) +
//! `storage.rs` (the Postgres read side) and registers them here via a
//! `pub fn register(registry: &mut ToolRegistry)`; DISC-03 adds `upsert.rs`
//! (the one write API every other item uses to mutate brochure rows).

pub mod schema;

pub use schema::{CandidateStatus, DiscoveryCandidate, FleetCategory};
