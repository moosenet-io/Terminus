//! LOCREG-01 tools: `location_set`, `location_list`, `location_clear`.
//!
//! These are how the registry gets filled in CONVERSATIONALLY — "I've moved",
//! "I'm in Denver this week", "forget the cabin" — instead of by editing config
//! on a host. They are the write half of the registry; the read half is the
//! consumer contract in [`crate::locations`], which weather/commute/news use
//! directly rather than by calling a tool.
//!
//! ## Storing is a deliberate act
//!
//! The weather ASK path (`crate::weather::location::ASK_MESSAGE`) invites the
//! user to say *"remember this is home"*, and that invitation is the natural
//! capture point. But answering a question is NOT consent to store the answer:
//! `weather` never writes here as a side effect of resolving a location. It
//! OFFERS; the user's next sentence is what reaches `location_set`. That
//! separation is why the offer can be made freely.
//!
//! ## Fail-closed dispatch
//!
//! Every tool here implements `execute` — the identity-less entry point — as a
//! refusal, and does its real work in `execute_with_caller_key`. A path that
//! forgets to thread a caller therefore gets "not available", never someone
//! else's record. The registry is keyed per caller, so there is no correct
//! answer to give a call that does not know who is asking.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::store::{FileLocationStore, LocationStore, StoredLocation};
use super::{
    clear, list, set, CallerKey, ClearOutcome, Listing, WriteOutcome, MAX_TEMPORARY_HOURS,
    WELL_KNOWN,
};
use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{CallerContext, RustTool, ToolOutput};

/// What every tool here says when it is reached without a caller identity, or
/// by a caller with no entitlement to stored-location context.
///
/// Deliberately identical in both cases, and deliberately silent about whether
/// anything is stored: "you're not allowed to see the home address that exists"
/// and "there is no home address" must be indistinguishable to someone who may
/// not have either fact.
const UNAVAILABLE: &str =
    "Saved locations aren't available on this connection. If you meant to save a place, \
     ask me again from your own session.";

/// The shared error text for a registry we could not read or write.
///
/// The one thing it must never do is read like "you have nothing saved" — that
/// is the confusion this whole item exists to prevent.
fn could_not_read(detail: &str) -> String {
    format!("I couldn't read your saved locations just now ({detail}), so I don't know what's there. This is a problem reading them, not an empty list — nothing has been changed.")
}

fn describe(name: &str, entry: &StoredLocation) -> String {
    match entry.expires_at_unix {
        None => format!("{name}: {}", entry.value),
        Some(t) => {
            let hours = ((t - chrono::Utc::now().timestamp()) as f64 / 3600.0).ceil().max(0.0);
            format!("{name}: {} (temporary, about {hours:.0}h left)", entry.value)
        }
    }
}

// ── location_set ────────────────────────────────────────────────────────────

struct LocationSet {
    store: Arc<dyn LocationStore>,
}

#[async_trait]
impl RustTool for LocationSet {
    fn name(&self) -> &str {
        "location_set"
    }

    fn description(&self) -> &str {
        "Save or update one of the user's named locations — 'home', 'work', 'current' (where they \
         are right now), or any name they choose ('the cabin', 'mum's house'). Use this when the \
         user asks to remember a place ('I've moved', 'remember this is home', 'I'm in Denver this \
         week'). Never call it to record a place the user merely mentioned in passing — saving is \
         something they ask for. Replacing an existing, different value requires confirm=true; call \
         once without it, tell the user what will be replaced, and call again once they agree."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["name", "location"],
            "properties": {
                "name": {
                    "type": "string",
                    "description": format!(
                        "What to call this place. Well-known names other tools understand: {}. \
                         Any other name is fine and is stored as given.",
                        WELL_KNOWN.join(", ")
                    ),
                },
                "location": {
                    "type": "string",
                    "description": "The place itself, as the user said it — a city, a full address, or 'lat,lon'.",
                },
                "expires_in_hours": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TEMPORARY_HOURS,
                    "description": "Makes this a TEMPORARY location that stops being used after this many hours. Use it for travel ('I'm in Denver this week' → name 'current', about 168). Omit for a permanent location.",
                },
                "confirm": {
                    "type": "boolean",
                    "description": "Set true only after the user has confirmed replacing an existing different value for this name.",
                }
            }
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        Ok(UNAVAILABLE.to_string())
    }

    async fn execute_with_caller_key(
        &self,
        args: Value,
        caller: CallerContext,
        key: Option<CallerKey>,
    ) -> Result<ToolOutput, ToolError> {
        let name = args.get("name").and_then(Value::as_str).unwrap_or("");
        let value = args.get("location").and_then(Value::as_str).unwrap_or("");
        let hours = args.get("expires_in_hours").and_then(Value::as_i64);
        let confirm = args.get("confirm").and_then(Value::as_bool).unwrap_or(false);

        let text = match set(self.store.as_ref(), key.as_ref(), caller, name, value, hours, confirm) {
            WriteOutcome::Stored { name, entry, replaced } => {
                let lead = match replaced {
                    Some(_) => format!("Updated {name}"),
                    None => format!("Saved {name}"),
                };
                match entry.expires_at_unix {
                    None => format!("{lead}: {}.", entry.value),
                    Some(_) => format!(
                        "{lead}: {} — temporarily, for about {}h. I'll stop using it after that rather than treating it as permanent.",
                        entry.value,
                        hours.unwrap_or(0)
                    ),
                }
            }
            WriteOutcome::NeedsConfirmation { name, existing_is_temporary } => format!(
                "You already have a {}{name} saved and it's different from that. Want me to replace it? \
                 (I won't change anything until you say so.)",
                if existing_is_temporary { "temporary " } else { "" }
            ),
            WriteOutcome::Rejected(why) => format!("I can't save that — {why}."),
            WriteOutcome::Denied => UNAVAILABLE.to_string(),
            WriteOutcome::Unavailable(e) => could_not_read(&e.to_string()),
        };
        Ok(ToolOutput::text_only(text))
    }
}

// ── location_list ───────────────────────────────────────────────────────────

struct LocationList {
    store: Arc<dyn LocationStore>,
}

#[async_trait]
impl RustTool for LocationList {
    fn name(&self) -> &str {
        "location_list"
    }

    fn description(&self) -> &str {
        "List the user's saved named locations (home, work, current, and any others), including any \
         that have expired. Use it to answer 'what do you have saved for me?' or before offering to \
         replace one. If nothing is saved it says so plainly — do NOT fill the gap with a guess."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        Ok(UNAVAILABLE.to_string())
    }

    async fn execute_with_caller_key(
        &self,
        _args: Value,
        caller: CallerContext,
        key: Option<CallerKey>,
    ) -> Result<ToolOutput, ToolError> {
        let text = match list(self.store.as_ref(), key.as_ref(), caller) {
            Listing::Entries { live, expired } if live.is_empty() && expired.is_empty() => {
                "You don't have any locations saved yet. Tell me where home is and I'll remember it."
                    .to_string()
            }
            Listing::Entries { live, expired } => {
                let mut out = String::from("Saved locations:\n");
                for (n, e) in &live {
                    out.push_str(&format!("- {}\n", describe(n, e)));
                }
                if !expired.is_empty() {
                    out.push_str("Expired (no longer used, tell me to clear them):\n");
                    for (n, e) in &expired {
                        out.push_str(&format!("- {n}: {}\n", e.value));
                    }
                }
                out
            }
            Listing::Denied => UNAVAILABLE.to_string(),
            Listing::Unavailable(e) => could_not_read(&e.to_string()),
        };
        Ok(ToolOutput::text_only(text))
    }
}

// ── location_clear ──────────────────────────────────────────────────────────

struct LocationClear {
    store: Arc<dyn LocationStore>,
}

#[async_trait]
impl RustTool for LocationClear {
    fn name(&self) -> &str {
        "location_clear"
    }

    fn description(&self) -> &str {
        "Forget a saved location. Give the name to remove one ('forget the cabin', 'I don't work \
         there any more' → name 'work'). Removing EVERYTHING requires all=true and should only be \
         used when the user has clearly asked for exactly that."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The saved location to forget."},
                "all": {
                    "type": "boolean",
                    "description": "Remove every saved location. Only when the user explicitly asked to forget them all.",
                }
            }
        })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        Ok(UNAVAILABLE.to_string())
    }

    async fn execute_with_caller_key(
        &self,
        args: Value,
        caller: CallerContext,
        key: Option<CallerKey>,
    ) -> Result<ToolOutput, ToolError> {
        let name = args.get("name").and_then(Value::as_str);
        let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);

        let text = match clear(self.store.as_ref(), key.as_ref(), caller, name, all) {
            ClearOutcome::Cleared { count } if count == 1 => "Forgotten.".to_string(),
            ClearOutcome::Cleared { count } => format!("Forgotten — all {count} saved locations."),
            ClearOutcome::NotSet => "There was nothing saved under that name.".to_string(),
            ClearOutcome::NeedsConfirmation => {
                "Which one should I forget? (Or say you want them all cleared and I'll do that.)"
                    .to_string()
            }
            ClearOutcome::Rejected(why) => format!("I can't do that — {why}."),
            ClearOutcome::Denied => UNAVAILABLE.to_string(),
            ClearOutcome::Unavailable(e) => could_not_read(&e.to_string()),
        };
        Ok(ToolOutput::text_only(text))
    }
}

// ── Registration ────────────────────────────────────────────────────────────

/// The process-wide registry store.
///
/// One `Arc` shared by the tools AND by every consumer (weather today), so a
/// location saved through `location_set` is visible to the next `weather` call
/// with no cache to invalidate.
pub fn shared_store() -> Arc<dyn LocationStore> {
    static STORE: std::sync::OnceLock<Arc<FileLocationStore>> = std::sync::OnceLock::new();
    STORE.get_or_init(|| Arc::new(FileLocationStore::from_env())).clone()
}

pub fn register(registry: &mut ToolRegistry) {
    let store = shared_store();
    registry.register_or_replace(Box::new(LocationSet { store: store.clone() }));
    registry.register_or_replace(Box::new(LocationList { store: store.clone() }));
    registry.register_or_replace(Box::new(LocationClear { store }));
}

#[cfg(test)]
mod tests {
    use super::super::store::fake::{BrokenStore, CountingStore};
    use super::super::{CURRENT, HOME, WORK};
    use super::*;

    fn entitled() -> CallerContext {
        CallerContext::entitled_for_test_only(false, true)
    }

    fn key() -> CallerKey {
        CallerKey::for_principal_name("alpha").unwrap()
    }

    const A_HOME: &str = "1 Placeholder Way, Examplecity"; // pii-test-fixture: obvious placeholder standing in for a home address

    #[tokio::test]
    async fn the_identityless_entry_point_never_touches_the_store() {
        // `execute` has no caller, so there is no correct record to answer from.
        let s = Arc::new(CountingStore::new());
        for t in [
            Box::new(LocationSet { store: s.clone() }) as Box<dyn RustTool>,
            Box::new(LocationList { store: s.clone() }),
            Box::new(LocationClear { store: s.clone() }),
        ] {
            let out = t.execute(json!({"name": "home", "location": "Anywhere"})).await.unwrap();
            assert_eq!(out, UNAVAILABLE);
        }
        assert_eq!(s.reads(), 0);
        assert_eq!(s.writes(), 0);
    }

    #[tokio::test]
    async fn set_list_clear_through_the_tools() {
        let s = Arc::new(CountingStore::new());
        let setter = LocationSet { store: s.clone() };
        let lister = LocationList { store: s.clone() };
        let clearer = LocationClear { store: s.clone() };

        let out = setter
            .execute_with_caller_key(
                json!({"name": "home", "location": A_HOME}),
                entitled(),
                Some(key()),
            )
            .await
            .unwrap();
        assert!(out.text.starts_with("Saved home"));

        let out = lister
            .execute_with_caller_key(json!({}), entitled(), Some(key()))
            .await
            .unwrap();
        assert!(out.text.contains(A_HOME));

        // Replacing needs confirmation first.
        let out = setter
            .execute_with_caller_key(
                json!({"name": "home", "location": "9 Elsewhere St"}), // pii-test-fixture: obvious placeholder standing in for a different home address
                entitled(),
                Some(key()),
            )
            .await
            .unwrap();
        assert!(out.text.contains("replace"), "got {}", out.text);

        let out = clearer
            .execute_with_caller_key(json!({"name": "home"}), entitled(), Some(key()))
            .await
            .unwrap();
        assert_eq!(out.text, "Forgotten.");
    }

    #[tokio::test]
    async fn an_empty_registry_and_a_broken_one_read_differently_to_the_user() {
        let empty = LocationList { store: Arc::new(CountingStore::new()) };
        let broken = LocationList { store: Arc::new(BrokenStore) };

        let a = empty.execute_with_caller_key(json!({}), entitled(), Some(key())).await.unwrap().text;
        let b = broken.execute_with_caller_key(json!({}), entitled(), Some(key())).await.unwrap().text;

        assert!(a.to_lowercase().contains("don't have any"), "got {a}");
        assert!(b.to_lowercase().contains("couldn't read"), "got {b}");
        assert!(
            !b.to_lowercase().contains("don't have any") && !b.contains("no locations"),
            "a read failure must not be phrased as an empty registry: {b}"
        );
    }

    #[tokio::test]
    async fn the_unentitled_answer_reveals_nothing_about_what_is_stored() {
        let s = Arc::new(CountingStore::new());
        let lister = LocationList { store: s.clone() };
        // Seed a value an unentitled caller must not learn about.
        super::set(s.as_ref(), Some(&key()), entitled(), "home", A_HOME, None, false);

        let out = lister
            .execute_with_caller_key(json!({}), CallerContext::default(), Some(key()))
            .await
            .unwrap();
        assert_eq!(out.text, UNAVAILABLE);
        assert!(!out.text.contains("Placeholder"));
    }

    #[test]
    fn the_schema_suggests_no_city() {
        // Same lesson as the weather schema: an example in a schema becomes a
        // default in practice, and "Tampa" reached a user that way.
        let s = Arc::new(CountingStore::new());
        let params = LocationSet { store: s }.parameters().to_string().to_lowercase();
        for city in ["tampa", "paris", "omaha", "san francisco", "foster city", "new york"] {
            assert!(!params.contains(city), "the schema must not seed a city, found {city:?}");
        }
        assert!(params.contains(HOME) && params.contains(WORK) && params.contains(CURRENT));
    }
}
