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
//! - Later: `tool`/`storage` (DISC-02), `upsert` (DISC-03), etc.

pub mod hf_client;
pub mod schema;

pub use schema::{CandidateStatus, DiscoveryCandidate, FleetCategory};
