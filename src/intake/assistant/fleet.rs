//! S84 ASMT-09 — Lumina fleet registration (dual-fleet, never clobbered).
//!
//! A model that survives the assistant suite is registered into the **Lumina**
//! fleet. Crucially, a model may ALSO be in the **Harmony** (S83 builder) fleet.
//! Fleet membership is recorded as ROWS, not as a mutable flag on the model, so a
//! dual-fleet model shows BOTH memberships as separate rows that join cleanly:
//!
//!   - a Harmony row: `dimension="fleet_membership"`, `metric="harmony"`,
//!   - a Lumina row:  `dimension="fleet_membership"`, `metric="lumina"`.
//!
//! ## No-clobber invariant (the load-bearing property)
//! Registration is **append-only**: it writes a NEW `assistant_dimension_score`
//! row via [`super::schema::insert_dimension_score`] (a plain INSERT — never an
//! UPDATE/UPSERT). A Lumina registration therefore CANNOT overwrite a Harmony
//! row: the two rows differ in their `metric` (fleet name) and are independent
//! INSERTs keyed on `(model_id, backend_tag, dimension, metric)`. The negative
//! test [`tests::lumina_write_never_clobbers_harmony`] proves a Lumina write
//! leaves a pre-existing Harmony row byte-identical.
//!
//! The storage call is abstracted behind [`FleetStore`] so the no-clobber proof
//! runs hermetically against an in-memory store that faithfully models the
//! append-only INSERT (it panics if anything ever tries to mutate an existing
//! row), independent of a live Postgres.

use super::{BackendTag, DimensionScore, ModelId};

/// The `dimension` value every fleet-membership row carries.
pub const DIMENSION: &str = "fleet_membership";

/// The `judge`/source label for a fleet-membership row (not a panel score).
pub const SOURCE: &str = "fleet";

/// Membership value: `1.0` registered. Kept numeric so it lives in the shared
/// `value DOUBLE PRECISION` column without a schema change.
pub const REGISTERED: f64 = 1.0;

/// The two fleets a model can belong to. S83/MINT owns `Harmony`; S84 owns
/// `Lumina`. A model may be in both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fleet {
    /// S83 builder fleet (Harmony's coders).
    Harmony,
    /// S84 assistant fleet (Lumina's chat models).
    Lumina,
}

impl Fleet {
    /// Stable lowercase tag stored in the `metric` column.
    pub fn tag(self) -> &'static str {
        match self {
            Fleet::Harmony => "harmony",
            Fleet::Lumina => "lumina",
        }
    }
}

/// Build the append-only fleet-membership row for `(model, backend, fleet)`.
///
/// This is a fresh row — the caller INSERTs it. It never references or updates
/// any other fleet's row, which is exactly what keeps Harmony and Lumina
/// memberships independent.
pub fn membership_row(
    model_id: &ModelId,
    backend_tag: BackendTag,
    fleet: Fleet,
    rationale: impl Into<String>,
) -> DimensionScore {
    DimensionScore {
        model_id: model_id.clone(),
        backend_tag,
        dimension: DIMENSION.to_string(),
        metric: fleet.tag().to_string(),
        value: REGISTERED,
        std_dev: None,
        judge: SOURCE.to_string(),
        low_confidence: false,
        raw_json: Some(
            serde_json::json!({ "fleet": fleet.tag(), "rationale": rationale.into() }).to_string(),
        ),
    }
}

/// Append-only fleet store. The ONE mutation it supports is "insert a new row";
/// there is no update path, by design — that is what guarantees no clobber.
#[async_trait::async_trait]
pub trait FleetStore: Send + Sync {
    /// Insert ONE fleet-membership row. Implementations MUST be pure inserts; an
    /// implementation that updated an existing row would violate the dual-fleet
    /// invariant.
    async fn insert_membership(&self, row: &DimensionScore) -> Result<(), String>;
}

/// Register `model`/`backend` into the **Lumina** fleet (the ASMT-09 survivor
/// path). Append-only: writes exactly one new row. Returns the row written so the
/// runner can checkpoint/audit it.
pub async fn register_lumina(
    store: &dyn FleetStore,
    model_id: &ModelId,
    backend_tag: BackendTag,
    rationale: impl Into<String>,
) -> Result<DimensionScore, String> {
    let row = membership_row(model_id, backend_tag, Fleet::Lumina, rationale);
    store.insert_membership(&row).await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory store that models the real INSERT semantics: rows are keyed on
    /// `(model_id, backend_tag, dimension, metric)` — the same identity tuple the
    /// Postgres rows carry. It PANICS if anything tries to insert over an existing
    /// key with a different payload, which would be a clobber. This makes the
    /// no-clobber property an executable assertion, not just a comment.
    #[derive(Default)]
    struct MemStore {
        rows: Mutex<HashMap<(String, String, String, String), DimensionScore>>,
    }

    impl MemStore {
        fn key(r: &DimensionScore) -> (String, String, String, String) {
            (
                r.model_id.as_str().to_string(),
                r.backend_tag.as_str().to_string(),
                r.dimension.clone(),
                r.metric.clone(),
            )
        }
        fn get(&self, key: &(String, String, String, String)) -> Option<DimensionScore> {
            self.rows.lock().unwrap().get(key).cloned()
        }
        fn len(&self) -> usize {
            self.rows.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl FleetStore for MemStore {
        async fn insert_membership(&self, row: &DimensionScore) -> Result<(), String> {
            let mut rows = self.rows.lock().unwrap();
            let key = MemStore::key(row);
            if let Some(existing) = rows.get(&key) {
                // Same identity → must be the same payload, else it's a clobber.
                assert_eq!(
                    existing, row,
                    "CLOBBER: insert over an existing fleet row with a different payload"
                );
            }
            rows.insert(key, row.clone());
            Ok(())
        }
    }

    fn seed_harmony(store: &MemStore, model: &ModelId, backend: BackendTag) -> DimensionScore {
        // Simulate S83 having already registered this model into Harmony.
        let row = membership_row(model, backend, Fleet::Harmony, "S83 builder survivor");
        futures_block_on(store.insert_membership(&row)).unwrap();
        row
    }

    #[test]
    fn lumina_write_never_clobbers_harmony() {
        // A model already in Harmony (S83). A Lumina registration must add a
        // SECOND row, leaving the Harmony row byte-identical.
        let store = MemStore::default();
        let model = ModelId::from("qwen3:8b");
        let harmony_row = seed_harmony(&store, &model, BackendTag::Gpu);
        let harmony_key = MemStore::key(&harmony_row);
        let before = store.get(&harmony_key).unwrap();

        let lumina_row =
            futures_block_on(register_lumina(&store, &model, BackendTag::Gpu, "S84 survivor"))
                .unwrap();

        // 1. The Harmony row is untouched.
        let after = store.get(&harmony_key).unwrap();
        assert_eq!(before, after, "Harmony row must not change after a Lumina write");
        // 2. Both fleet rows coexist as separate rows.
        assert_eq!(store.len(), 2);
        assert_eq!(harmony_row.metric, "harmony");
        assert_eq!(lumina_row.metric, "lumina");
        // 3. Same model + backend, different fleet metric → independent identities.
        assert_eq!(lumina_row.model_id, harmony_row.model_id);
        assert_eq!(lumina_row.backend_tag, harmony_row.backend_tag);
        assert_ne!(MemStore::key(&lumina_row), harmony_key);
    }

    #[test]
    fn dual_fleet_on_both_backends_yields_four_rows() {
        // A model in both fleets on both backends → 4 distinct membership rows.
        let store = MemStore::default();
        let model = ModelId::from("mixtral:8x7b");
        seed_harmony(&store, &model, BackendTag::Gpu);
        seed_harmony(&store, &model, BackendTag::Cpu);
        futures_block_on(register_lumina(&store, &model, BackendTag::Gpu, "r")).unwrap();
        futures_block_on(register_lumina(&store, &model, BackendTag::Cpu, "r")).unwrap();
        assert_eq!(store.len(), 4);
    }

    #[test]
    fn re_registering_same_fleet_is_idempotent_not_a_clobber() {
        // Registering the same (model, backend, fleet) twice with the same payload
        // is allowed (idempotent insert), and must NOT trip the clobber guard.
        let store = MemStore::default();
        let model = ModelId::from("gemma3:12b");
        futures_block_on(register_lumina(&store, &model, BackendTag::Gpu, "same")).unwrap();
        futures_block_on(register_lumina(&store, &model, BackendTag::Gpu, "same")).unwrap();
        assert_eq!(store.len(), 1);
    }

    fn futures_block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }
}
