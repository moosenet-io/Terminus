//! Model Discovery & Curation Agent (S114, the "Brochure").
//!
//! Module root — kept intentionally minimal (a single `pub mod` line) since
//! DISC-01 (Postgres schema/storage) may land in parallel and add sibling
//! modules (`schema`, `storage`, `tool`, `upsert`) plus re-exports here. See
//! `S114-model-discovery-agent-spec.md`'s Grounding summary for the full
//! brochure design (Postgres table `model_discovery_candidate`, MCP tool
//! `model_discovery_brochure`) — this item (DISC-04) only adds the HF Hub
//! listing client that a later DISC-05/DISC-06 classifies and persists.

pub mod hf_client;
