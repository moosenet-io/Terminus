//! The single authority on which GPU backends may be **stopped** to free the GPU.
//!
//! ## The defect this module exists to make impossible (CHRD #112)
//! `lifecycle::free_gpu` used to iterate `infer::gpu_backends()` — every backend
//! in Chord's `model-registry.json` with `hardware == "gpu"` — and
//! `systemctl stop` each one that was not the backend being started. That list
//! included the **always-on** primary Ollama serve, which Chord seeds as
//! `hardware: "gpu", unit: Some("ollama.service"), always_on: true`. So a cold
//! start of ANY on-demand GPU backend ran `systemctl stop ollama.service` and
//! took the live assistant's own inference engine down to make room.
//!
//! ## Why a type, and not a check
//! Chord shipped a defensive refusal on its side (RVXR-01): its coder tier
//! declined to load at all while any always-on GPU unit was registered. That is a
//! shield, not a fix — it makes one caller inert and leaves every other caller of
//! `free_gpu` exposed, and it can only ever be a CHECK racing the file re-read
//! that `free_gpu` itself performs.
//!
//! The fix here is structural: a stop needs a [`StoppableGpuBackend`], its fields
//! are private, and the **only** way to obtain one is
//! [`stoppable_gpu_backends_from_json`], which applies the guard while parsing.
//! A new caller written by someone who has never heard of this bug cannot express
//! "stop the assistant's engine", because it cannot get hold of a value that says
//! so. There is nothing to remember.
//!
//! The parsing core is pure (raw JSON text in, decisions out) so the rule can be
//! tested exhaustively without a filesystem, a registry, or — most importantly —
//! ever running `systemctl` against a live host.

use std::collections::BTreeMap;

use serde::Deserialize;

/// A GPU backend that has passed the never-stop guard and may therefore be
/// stopped to free the GPU.
///
/// Fields are private and there is no public constructor: values of this type
/// exist only where [`stoppable_gpu_backends_from_json`] made them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoppableGpuBackend {
    name: String,
    unit: Option<String>,
}

impl StoppableGpuBackend {
    /// Registry name of the backend (also the transient `chord-<name>` unit stem).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Declared systemd unit, if the backend has one (`None` ⇒ it only ever runs
    /// as a transient `chord-<name>` unit).
    pub fn unit(&self) -> Option<&str> {
        self.unit.as_deref()
    }
}

/// **The safety rule, written once.** An `always_on` backend is never stopped by
/// anything in this crate: it is the live assistant's own engine, and stopping it
/// to reclaim capacity for a background job is the single most damaging thing the
/// lifecycle layer could do.
///
/// Deliberately narrow: this is the SAFETY rule, not the lifecycle rule.
/// `lifecycle::stop` additionally declines `ollama`/`daemon` *kinds* because they
/// have no process for this crate to manage — a different question, kept
/// separate on purpose. Folding them together here would silently change
/// `free_gpu` from "stop every other GPU holder" to "stop only the unit-managed
/// ones", which would leave a GPU-holding daemon resident. One rule, one place,
/// one meaning.
pub fn may_stop(always_on: bool) -> bool {
    !always_on
}

#[derive(Deserialize)]
struct RegFile {
    #[serde(default)]
    backends: BTreeMap<String, RegBackend>,
}

#[derive(Deserialize)]
struct RegBackend {
    #[serde(default)]
    hardware: Option<String>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    always_on: bool,
}

/// Pure core: given the raw `model-registry.json` text and the backend being
/// started (`keep`), the GPU backends that may be stopped to free the GPU for it.
///
/// Excluded, and each for its own reason:
/// - `keep` itself — never stop the thing you are starting;
/// - `always_on` backends — [`may_stop`], the assistant's engine;
/// - non-GPU backends — they are not holding the GPU.
///
/// A missing/unparseable registry yields an EMPTY list, so the caller stops
/// nothing. That is the safe direction: without a registry we cannot know what is
/// safe to stop, and doing nothing never takes a service down.
pub fn stoppable_gpu_backends_from_json(raw: &str, keep: &str) -> Vec<StoppableGpuBackend> {
    let Ok(reg) = serde_json::from_str::<RegFile>(raw) else {
        return Vec::new();
    };
    reg.backends
        .into_iter()
        .filter(|(name, b)| {
            name != keep && b.hardware.as_deref() == Some("gpu") && may_stop(b.always_on)
        })
        .map(|(name, b)| StoppableGpuBackend {
            name,
            unit: b.unit,
        })
        .collect()
}

/// Test-only source scanner backing the ratchet in
/// [`crate::intake::lifecycle`]'s test module.
///
/// The type guard makes it impossible to *ask* `free_gpu` to stop an always-on
/// backend, but it cannot stop someone adding a NEW function that shells out
/// `systemctl stop` against a name read straight from the registry — which is
/// exactly how the original defect (CHRD #112) was written. The ratchet pins the
/// set of functions allowed to issue a stop at all.
///
/// It lives here, pure and separately tested, because a ratchet whose own parser
/// is wrong is worse than no ratchet: it reports "no new stop sites" forever.
#[cfg(test)]
pub fn stop_call_site_owners(src: &str) -> Vec<String> {
    // Item-level `fn` spans, in source order: (name, byte offset of `fn`).
    let mut fns: Vec<(&str, usize)> = Vec::new();
    for (idx, _) in src.match_indices("fn ") {
        // Only item-level fns: what precedes `fn` on its line must be nothing,
        // `pub`, `async`, or `pub async` — never an `Fn(` bound or prose.
        let line_start = src[..idx].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let prefix = src[line_start..idx].trim();
        if !matches!(prefix, "" | "pub" | "async" | "pub async") {
            continue;
        }
        let rest = &src[idx + 3..];
        let name_end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        fns.push((&rest[..name_end], idx));
    }

    // Every `systemctl` + `stop` argv, attributed to its enclosing fn. The needle
    // is assembled at runtime so this file never contains it literally (a
    // self-match would make the ratchet report a phantom site).
    let needle = format!("{}, {}", "\"systemctl\"", "\"stop\"");
    let mut owners: Vec<String> = Vec::new();
    for (idx, _) in src.match_indices(needle.as_str()) {
        let owner = fns
            .iter()
            .rev()
            .find(|(_, start)| *start < idx)
            .map(|(name, _)| (*name).to_string())
            .unwrap_or_else(|| "<top level>".to_string());
        if !owners.contains(&owner) {
            owners.push(owner);
        }
    }
    owners
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape Chord seeds on the live GPU host: the primary Ollama serve
    /// is `gpu` + `always_on` + a real unit, alongside an on-demand GPU backend.
    const LIVE_SHAPE: &str = r#"{
        "backends": {
            "ollama":    {"url":"http://127.0.0.1:11434","kind":"ollama","hardware":"gpu","unit":"ollama.service","always_on":true},
            "llama-gpu": {"url":"http://127.0.0.1:8082","kind":"llama-server","hardware":"gpu","always_on":false},
            "lemonade":  {"url":"http://127.0.0.1:8081","kind":"llama-server","hardware":"gpu","unit":"lemonade-coder.service","always_on":false},
            "ollama-cpu":{"url":"http://127.0.0.1:11435","kind":"ollama","hardware":"cpu","always_on":true},
            "llama-cpu": {"url":"http://127.0.0.1:8084","kind":"llama-server","hardware":"cpu","unit":"chord-llama-cpu.service","always_on":false}
        }
    }"#;

    /// THE defect (CHRD #112): starting an on-demand GPU backend must never
    /// produce `ollama.service` as something to stop.
    #[test]
    fn always_on_gpu_backend_is_never_stoppable() {
        let got = stoppable_gpu_backends_from_json(LIVE_SHAPE, "llama-gpu");
        assert!(
            !got.iter().any(|b| b.name() == "ollama"),
            "the always-on assistant engine must never appear in the stop set: {got:?}"
        );
        assert!(
            !got.iter().any(|b| b.unit() == Some("ollama.service")),
            "ollama.service must never appear as a unit to stop: {got:?}"
        );
    }

    /// The control, and it matters as much as the guard: a filter that returned
    /// nothing would also pass the assertion above while making GPU arbitration
    /// inert (every backend perpetually holding the GPU).
    #[test]
    fn on_demand_gpu_backends_are_still_stoppable() {
        let got = stoppable_gpu_backends_from_json(LIVE_SHAPE, "llama-gpu");
        let names: Vec<&str> = got.iter().map(|b| b.name()).collect();
        assert_eq!(names, vec!["lemonade"], "got {got:?}");
        assert_eq!(got[0].unit(), Some("lemonade-coder.service"));
    }

    #[test]
    fn the_backend_being_started_is_not_stopped() {
        let got = stoppable_gpu_backends_from_json(LIVE_SHAPE, "lemonade");
        let names: Vec<&str> = got.iter().map(|b| b.name()).collect();
        assert_eq!(names, vec!["llama-gpu"], "got {got:?}");
        assert_eq!(got[0].unit(), None, "no declared unit ⇒ transient unit only");
    }

    /// A CPU backend is not holding the GPU, so freeing the GPU must not touch
    /// it. `llama-cpu` is deliberately ON-DEMAND: an always-on CPU backend would
    /// be excluded by the `always_on` rule anyway and could not tell us whether
    /// the hardware filter is doing anything (it silently did not, in the first
    /// draft of this fixture — the mutation that dropped the hardware filter
    /// survived until `llama-cpu` was added).
    #[test]
    fn cpu_backends_are_not_stopped() {
        let got = stoppable_gpu_backends_from_json(LIVE_SHAPE, "llama-gpu");
        assert!(!got.iter().any(|b| b.name() == "ollama-cpu"), "{got:?}");
        assert!(
            !got.iter().any(|b| b.name() == "llama-cpu"),
            "an on-demand CPU backend is not a GPU holder: {got:?}"
        );
    }

    /// An always-on backend stays excluded even when it is the ONLY entry — i.e.
    /// the exclusion is a property of `always_on`, not an artifact of some other
    /// entry winning.
    #[test]
    fn always_on_only_registry_yields_nothing_to_stop() {
        let raw = r#"{"backends":{"ollama":{"url":"u","hardware":"gpu","unit":"ollama.service","always_on":true}}}"#;
        assert!(stoppable_gpu_backends_from_json(raw, "llama-gpu").is_empty());
    }

    #[test]
    fn missing_or_unparseable_registry_stops_nothing() {
        assert!(stoppable_gpu_backends_from_json("", "x").is_empty());
        assert!(stoppable_gpu_backends_from_json("not json", "x").is_empty());
        assert!(stoppable_gpu_backends_from_json("{}", "x").is_empty());
        assert!(stoppable_gpu_backends_from_json(r#"{"backends":{}}"#, "x").is_empty());
    }

    /// `always_on` absent from the JSON means `false` (serde default) — an
    /// on-demand backend written without the field is still stoppable, so the
    /// guard does not accidentally freeze GPU arbitration on older registries.
    #[test]
    fn absent_always_on_field_defaults_to_stoppable() {
        let raw = r#"{"backends":{"llama-gpu":{"url":"u","hardware":"gpu","unit":"chord-llama.service"}}}"#;
        let got = stoppable_gpu_backends_from_json(raw, "other");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name(), "llama-gpu");
    }

    #[test]
    fn may_stop_is_the_always_on_rule() {
        assert!(!may_stop(true));
        assert!(may_stop(false));
    }

    // ── the ratchet's own parser ────────────────────────────────────────────

    /// A miniature of the real module. The near-misses are the point: a `fn `
    /// inside a COMMENT and a `fn` POINTER TYPE, both sitting between a real
    /// function's header and its stop site — so a scanner that stops
    /// distinguishing item-level `fn`s misattributes the site to `reap_all`
    /// instead of `stop`, and the tests below say so.
    const SAMPLE: &str = r#"
/// docs mentioning fn free_gpu in prose
pub fn stop(b: &B) {
    // mirrors fn reap_all in the pre-guard code
    let hook: fn(u8) -> u8 = |x| x;
    let _ = run(["systemctl", "stop", unit]);
}

fn helper<F: Fn(u8) -> u8>(f: F) -> u8 { f(1) }

pub async fn ensure_up(b: &B) {
    let _ = run(["systemctl", "stop", &unit_name]);
}
"#;

    #[test]
    fn ratchet_attributes_stop_sites_to_their_enclosing_fn() {
        assert_eq!(
            stop_call_site_owners(SAMPLE),
            vec!["stop".to_string(), "ensure_up".to_string()]
        );
    }

    /// The ratchet's whole job: a NEW function that stops a unit must show up.
    #[test]
    fn ratchet_catches_a_new_unguarded_stop_site() {
        let rogue = format!(
            "{SAMPLE}\nfn reap_everything(name: &str) {{\n    let _ = run([\"systemctl\", \"stop\", name]);\n}}\n"
        );
        assert_eq!(
            stop_call_site_owners(&rogue),
            vec![
                "stop".to_string(),
                "ensure_up".to_string(),
                "reap_everything".to_string()
            ],
            "a new stop site must be attributed, not swallowed"
        );
    }

    /// A parser that found nothing would pass every allow-list assertion forever.
    /// The real ratchet asserts non-emptiness for exactly this reason; assert here
    /// that "no stop sites" is a state this function can actually report.
    #[test]
    fn ratchet_reports_nothing_when_there_is_nothing() {
        assert!(stop_call_site_owners("fn quiet() { let _ = 1; }").is_empty());
    }
}
