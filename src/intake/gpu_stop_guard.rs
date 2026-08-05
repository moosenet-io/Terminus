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
//!
//! ## Known residual: the guard trusts the registry
//! The rule is now TWO independent signals — `always_on` and the backend `kind`
//! (see [`is_unmanaged_kind`]) — so an entry has to misdescribe itself twice
//! before the assistant's engine becomes stoppable. What follows is the residual
//! that remains after both.
//!
//! ### It still trusts the registry's own fields
//! Raised in review (opus) and acknowledged rather than fixed. An entry written as
//! `always_on: false, unit: "ollama.service"` would be approved, and `free_gpu`
//! would stop it. Nothing here can prevent that: the registry is the only
//! statement this crate has of which backends are always-on, and a unit-name
//! denylist would be a second, weaker source of truth — wrong for every fleet
//! whose assistant engine is not literally named `ollama.service`. The guard makes
//! a TRUTHFUL registry unstoppable; a registry that misdescribes itself is a
//! different defect, in whatever wrote it. Chord seeds `always_on: true`, and the
//! pre-existing code was strictly worse — it ignored the flag entirely.

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

/// A backend KIND this crate never process-manages: an `ollama` serve's lifecycle
/// belongs to its own unit and to `gpu_authority`, never to backend arbitration.
///
/// This is a SECOND, INDEPENDENT signal from the same registry, and it exists
/// because reviewers (codex, gpt56) kept pressing on the one residual the
/// `always_on` flag alone cannot cover: an entry that misdescribes the assistant
/// engine as `always_on: false`. Requiring the entry to lie about BOTH its flag
/// AND its kind before it becomes stoppable is materially harder to do by
/// accident, and it costs nothing real — `lifecycle::stop` has ALWAYS refused
/// `ollama`-kind backends, so `free_gpu` stopping one was already the odd one out.
///
/// `daemon` kinds deliberately stay stoppable BY `free_gpu` (though not by
/// `stop`): a GPU-holding daemon must be evictable to free the GPU, and it is not
/// the assistant's engine.
pub fn is_unmanaged_kind(kind: Option<&str>) -> bool {
    matches!(kind.map(str::trim), Some("ollama"))
}

/// The transient systemd unit a launch-based backend runs as when it has no
/// declared unit. Defined HERE, not in `lifecycle`, because the guard has to
/// reason about it: `free_gpu` stops a candidate's declared unit AND its
/// transient one, so both are stop TARGETS and both must be checked. Two copies
/// of this format string would be two chances for the guard and the effect to
/// disagree about what they are naming.
pub fn transient_unit(backend: &str) -> String {
    format!("chord-{backend}.service")
}

/// Every systemd unit that must never be stopped, given the raw registry text:
/// for each backend this guard refuses to stop, both its DECLARED unit and its
/// TRANSIENT unit.
///
/// The transient half is round-5 (codex), and it needed no lie at all to exploit:
/// an always-on backend declaring `unit: "chord-evict.service"` alongside an
/// ordinary on-demand backend NAMED `evict` meant the candidate passed every
/// filter and then `free_gpu` stopped the always-on backend as the candidate's
/// own transient unit. Collisions are checked against stop TARGETS, not against
/// entries.
pub fn protected_units_from_json(raw: &str) -> std::collections::BTreeSet<String> {
    let Ok(reg) = serde_json::from_str::<RegFile>(raw) else {
        return Default::default();
    };
    reg.backends
        .iter()
        .filter(|(_, b)| !may_stop(b.always_on) || is_unmanaged_kind(b.kind.as_deref()))
        .flat_map(|(name, b)| {
            b.unit
                .clone()
                .into_iter()
                .chain(std::iter::once(transient_unit(name)))
        })
        .collect()
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
    kind: Option<String>,
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
/// - `ollama`-kind backends — [`is_unmanaged_kind`], a second independent signal
///   for the same thing, so a registry has to lie twice to become dangerous;
/// - anything whose stop TARGETS (declared unit or transient `chord-<name>` unit)
///   collide with a unit the first two rules protected — a protected unit is
///   protected under every alias, so neither a second entry naming
///   `ollama.service` nor a backend whose transient unit happens to be a
///   protected backend's declared unit can launder a stop through;
/// - non-GPU backends — they are not holding the GPU.
///
/// A missing/unparseable registry yields an EMPTY list, so the caller stops
/// nothing. That is the safe direction: without a registry we cannot know what is
/// safe to stop, and doing nothing never takes a service down.
pub fn stoppable_gpu_backends_from_json(raw: &str, keep: &str) -> Vec<StoppableGpuBackend> {
    let Ok(reg) = serde_json::from_str::<RegFile>(raw) else {
        return Vec::new();
    };

    // PROTECTED UNITS: every stop TARGET belonging to a backend this guard refuses
    // to stop — declared and transient alike. No candidate may name one, whatever
    // entry it arrives under.
    //
    // This closes two review findings at once: a SECOND entry of an innocuous kind
    // naming `unit: "ollama.service"` (r4, gpt56), and a candidate whose TRANSIENT
    // unit collides with a protected backend's declared unit (r5, codex). Neither
    // is a unit-name denylist and neither invents a second source of truth: the
    // protected set is derived from THIS registry, so the rule is just "the
    // registry must be self-consistent".
    let protected = protected_units_from_json(raw);

    reg.backends
        .iter()
        .filter(|(name, b)| {
            name.as_str() != keep
                && b.hardware.as_deref() == Some("gpu")
                && may_stop(b.always_on)
                && !is_unmanaged_kind(b.kind.as_deref())
                // BOTH stop targets: `free_gpu` stops the declared unit and the
                // transient one, so a collision on either is disqualifying.
                //
                // DELIBERATE REDUNDANCY, measured, not assumed: because every
                // protected entry contributes its OWN transient unit to
                // `protected`, the two clauses above (`may_stop`,
                // `!is_unmanaged_kind`) are already implied by the collision check
                // — a mutant deleting either one survives the suite. They are kept
                // explicit anyway: a safety rule that holds only as an emergent
                // consequence of a different rule is one refactor away from
                // silently not holding. The RULES themselves are load-bearing and
                // proven so (mutating `may_stop` kills 6 tests, mutating
                // `is_unmanaged_kind` kills 2); it is only their restatement here
                // that is redundant.
                && !b.unit.as_deref().is_some_and(|u| protected.contains(u))
                && !protected.contains(&transient_unit(name))
        })
        .map(|(name, b)| StoppableGpuBackend {
            name: name.clone(),
            unit: b.unit.clone(),
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
/// Test-only: strip comments and normalise whitespace, keeping a map from each
/// retained byte back to its offset in `src`.
///
/// Comments are removed with a small string-aware state machine rather than a
/// regex: round 2 of review showed that `pub /* c */ fn rogue()` and
/// `fn /* c */ rogue()` are valid Rust the previous scanner could not see, so the
/// stop inside them was attributed to the previous allow-listed function. Removing
/// comments before scanning deletes that whole class rather than adding another
/// special case. A naive strip would eat `"http://…"`, hence the string tracking.
///
/// Raw strings (`r#"…"#`) are treated as ordinary strings. That is deliberately
/// conservative: it can only RETAIN text, never hide it.
#[cfg(test)]
fn strip_comments_and_normalize(src: &str) -> (String, Vec<usize>) {
    #[derive(PartialEq)]
    enum St {
        Code,
        Str,
        Line,
        Block,
    }
    let bytes: Vec<(usize, char)> = src.char_indices().collect();
    let mut out = String::with_capacity(src.len());
    let mut map: Vec<usize> = Vec::with_capacity(src.len());
    let mut st = St::Code;
    let mut in_ws = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let (off, c) = bytes[i];
        let next = bytes.get(i + 1).map(|(_, c)| *c);
        match st {
            St::Code => {
                if c == '/' && next == Some('/') {
                    st = St::Line;
                    i += 2;
                    in_ws = true;
                    continue;
                }
                if c == '/' && next == Some('*') {
                    st = St::Block;
                    i += 2;
                    in_ws = true;
                    continue;
                }
                if c == '"' {
                    st = St::Str;
                }
            }
            St::Str => {
                if c == '\\' {
                    // Escape: copy both chars verbatim, never end the string on `\"`.
                    for k in 0..2 {
                        if let Some((o, ch)) = bytes.get(i + k) {
                            out.push(*ch);
                            map.push(*o);
                        }
                    }
                    i += 2;
                    in_ws = false;
                    continue;
                }
                if c == '"' {
                    st = St::Code;
                }
            }
            St::Line => {
                if c == '\n' {
                    st = St::Code;
                }
                i += 1;
                continue;
            }
            St::Block => {
                if c == '*' && next == Some('/') {
                    st = St::Code;
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
        }
        if c.is_whitespace() {
            in_ws = true;
            i += 1;
            continue;
        }
        if in_ws && !out.is_empty() {
            // A space after a comma carries no meaning in an argv literal; drop it
            // so `"systemctl", "stop"` and `"systemctl","stop"` scan identically.
            if !out.ends_with(',') {
                out.push(' ');
                map.push(off);
            }
        }
        in_ws = false;
        out.push(c);
        map.push(off);
        i += 1;
    }
    (out, map)
}

/// Test-only: how many `"systemctl"` STRING LITERALS the source contains.
///
/// The formatting/syntax-independent half of the ratchet, and the answer to the
/// arms race round 2 exposed. A lexical scanner will never attribute every legal
/// Rust spelling of a function header, and it cannot see
/// `let verb = "stop"; run(["systemctl", verb, u])` at all. But every one of those
/// bypasses still has to name `systemctl` somewhere, so pinning the COUNT catches
/// them all — including ones nobody has thought of — at the cost of also firing on
/// a legitimate new invocation, which is exactly when a human should look.
///
/// ## What this deliberately does NOT catch (codex, review round 4)
/// An author who WANTS to hide a stop can:
/// `run([&("system".to_owned() + "ctl"), &("st".to_owned() + "op"), u])`. No
/// literal, no census movement. That is sabotage, not the accident this ratchet
/// exists to catch, and no `#[cfg(test)]` lexical check survives an adversary
/// willing to obfuscate — chasing it is an arms race with no end state. Held
/// knowingly, and it is not the load-bearing protection anyway: the TYPE is.
/// Even obfuscated code cannot obtain an always-on backend from this module; it
/// would have to hardcode the unit name, which is a different and far more
/// visible thing to write.
#[cfg(test)]
pub fn systemctl_literal_count(src: &str) -> usize {
    let (norm, _) = strip_comments_and_normalize(src);
    norm.matches("\"systemctl\"").count()
}

/// Test-only source scanner backing the ratchet in
/// [`crate::intake::lifecycle`]'s test module: which functions issue a
/// `systemctl stop`.
///
/// This is the INFORMATIVE half — it names the offending function. It is paired
/// with [`systemctl_literal_count`], which is the half that cannot be evaded by
/// clever syntax. Neither is sufficient alone; a change that slips past one trips
/// the other.
#[cfg(test)]
pub fn stop_call_site_owners(src: &str) -> Vec<String> {
    let (norm, map) = strip_comments_and_normalize(src);

    // Item-level `fn` headers, in source order: (name, ORIGINAL byte offset).
    let mut fns: Vec<(String, usize)> = Vec::new();
    for (nidx, _) in norm.match_indices("fn ") {
        let orig = map[nidx];
        let line_start = src[..orig].rfind('\n').map(|p| p + 1).unwrap_or(0);
        // The prefix is taken from the NORMALISED text back to the start of the
        // line, so a comment between modifiers is already gone.
        let raw_prefix = &src[line_start..orig];
        let (norm_prefix, _) = strip_comments_and_normalize(raw_prefix);
        if !is_item_fn_prefix(&norm_prefix) {
            continue;
        }
        // The NAME is read from the normalised text too, so `fn /* c */ rogue()`
        // still yields `rogue`.
        let rest = &norm[nidx + 3..];
        let name_end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let name = rest[..name_end].trim().to_string();
        if !name.is_empty() {
            fns.push((name, orig));
        }
    }

    // Every `systemctl` + `stop` argv, attributed to its enclosing fn. The needle
    // is assembled at runtime so this file never contains it literally (a
    // self-match would make the ratchet report a phantom site).
    let needle = format!("{},{}", "\"systemctl\"", "\"stop\"");
    let mut owners: Vec<String> = Vec::new();
    for (nidx, _) in norm.match_indices(needle.as_str()) {
        let orig = map[nidx];
        let owner = fns
            .iter()
            .rev()
            .find(|(_, start)| *start < orig)
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| "<top level>".to_string());
        if !owners.contains(&owner) {
            owners.push(owner);
        }
    }
    owners
}

/// Does what precedes `fn` on its line consist ONLY of things that can legally
/// precede an ITEM-level function — i.e. is this a real function header rather
/// than a `fn` inside prose, a `fn(..)` pointer type, or a trait bound?
///
/// Written as "every token must be a permitted modifier" rather than an allow-list
/// of whole prefixes. The whole-prefix version accepted exactly
/// `""`/`"pub"`/`"async"`/`"pub async"`, so `pub(crate) fn`, `const fn`,
/// `unsafe fn`, `extern "C" fn` and `#[inline] fn` were all invisible to it — and
/// a stop site inside such a function was silently attributed to the previous,
/// allow-listed function. Three reviewers found this independently; it was real.
///
/// Expects COMMENT-STRIPPED input (see [`strip_comments_and_normalize`]).
#[cfg(test)]
fn is_item_fn_prefix(prefix: &str) -> bool {
    let mut p = prefix.trim();
    // Attributes on the same line: `#[inline] pub fn …`.
    while let Some(stripped) = p.strip_prefix("#[") {
        match stripped.find(']') {
            Some(i) => p = stripped[i + 1..].trim_start(),
            None => return false,
        }
    }
    // Restricted visibility: `pub(crate)`, `pub(super)`, `pub(in path)`.
    if let Some(stripped) = p.strip_prefix("pub(") {
        match stripped.find(')') {
            Some(i) => p = stripped[i + 1..].trim_start(),
            None => return false,
        }
    }
    p.split_whitespace().all(|t| {
        matches!(t, "pub" | "async" | "unsafe" | "const" | "default" | "extern")
            || t.starts_with('"') // the ABI string in `extern "C"`
    })
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
            "pinned-gpu":{"url":"http://127.0.0.1:8085","kind":"llama-server","hardware":"gpu","unit":"pinned.service","always_on":true},
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
        // `pinned-gpu` is excluded by `always_on` ALONE — it is a llama-server, so
        // the kind rule does not reach it. Without this case the headline
        // assertion would be satisfied by the kind rule and would no longer
        // discriminate the always_on rule at all (a mutant proved exactly that).
        assert!(
            !got.iter().any(|b| b.name() == "pinned-gpu"),
            "an always-on llama-server backend must be excluded by always_on alone: {got:?}"
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

    /// The residual codex and gpt56 pressed on across two rounds: an entry that
    /// misdescribes the assistant engine as `always_on: false`. It is now
    /// excluded by the SECOND signal (kind), so the registry has to lie twice.
    #[test]
    fn a_mislabelled_ollama_engine_is_still_not_stoppable() {
        let lying = r#"{"backends":{
            "ollama":{"url":"http://x","kind":"ollama","hardware":"gpu",
                      "unit":"ollama.service","always_on":false}}}"#;
        assert!(
            stoppable_gpu_backends_from_json(lying, "llama-gpu").is_empty(),
            "an ollama-kind GPU serve is never process-managed here, whatever its \
             always_on flag claims"
        );

        // The same lie with NO unit — so only the KIND rule can exclude it. Without
        // this case the protected-unit cross-check masks the kind rule entirely and
        // a mutant that deletes the kind rule survives (it did, once).
        let lying_unitless = r#"{"backends":{
            "ollama":{"url":"http://x","kind":"ollama","hardware":"gpu",
                      "always_on":false}}}"#;
        assert!(
            stoppable_gpu_backends_from_json(lying_unitless, "llama-gpu").is_empty(),
            "an ollama-kind backend is excluded by its KIND alone, with no unit to \
             protect and no always_on flag to trust"
        );

        // CONTROL: the kind rule must not swallow everything. A daemon-kind GPU
        // backend IS still stoppable by free_gpu — it holds the GPU and is not the
        // assistant's engine. (`lifecycle::stop` separately declines it; that
        // asymmetry is deliberate and documented on `is_unmanaged_kind`.)
        let daemon = r#"{"backends":{
            "dgem":{"url":"http://x","kind":"daemon","hardware":"gpu",
                    "unit":"dgem.service","always_on":false}}}"#;
        let got = stoppable_gpu_backends_from_json(daemon, "llama-gpu");
        assert_eq!(got.len(), 1, "a GPU-holding daemon must stay evictable: {got:?}");
        assert_eq!(got[0].name(), "dgem");
    }

    /// Round-4 (gpt56): a SECOND entry, innocuous kind, `always_on: false`, that
    /// names the protected unit. Closed by the protected-unit cross-check.
    #[test]
    fn a_protected_unit_cannot_be_laundered_through_another_entry() {
        let laundered = r#"{"backends":{
            "ollama":{"url":"http://x","kind":"ollama","hardware":"gpu",
                      "unit":"ollama.service","always_on":true},
            "innocuous":{"url":"http://y","kind":"llama-server","hardware":"gpu",
                         "unit":"ollama.service","always_on":false}}}"#;
        let got = stoppable_gpu_backends_from_json(laundered, "llama-gpu");
        assert!(
            got.is_empty(),
            "a unit protected under one entry is protected under every entry: {got:?}"
        );

        // CONTROL: the cross-check must only protect units that are ACTUALLY
        // protected. The same second entry, naming its own unit, is stoppable —
        // otherwise this rule would quietly disable GPU arbitration.
        let ordinary = r#"{"backends":{
            "ollama":{"url":"http://x","kind":"ollama","hardware":"gpu",
                      "unit":"ollama.service","always_on":true},
            "innocuous":{"url":"http://y","kind":"llama-server","hardware":"gpu",
                         "unit":"innocuous.service","always_on":false}}}"#;
        let got = stoppable_gpu_backends_from_json(ordinary, "llama-gpu");
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].unit(), Some("innocuous.service"));
    }

    /// Round-5 (codex): the exploit that needed NO lie. `free_gpu` stops a
    /// candidate's TRANSIENT unit as well as its declared one, so an always-on
    /// backend that declares `unit: "chord-evict.service"` was stoppable as
    /// on-demand backend `evict`'s transient unit. Everything here is truthful.
    #[test]
    fn a_transient_unit_collision_cannot_stop_a_protected_backend() {
        let colliding = r#"{"backends":{
            "pinned":{"url":"http://x","kind":"llama-server","hardware":"gpu",
                      "unit":"chord-evict.service","always_on":true},
            "evict":{"url":"http://y","kind":"llama-server","hardware":"gpu",
                     "always_on":false}}}"#;
        let got = stoppable_gpu_backends_from_json(colliding, "llama-gpu");
        assert!(
            got.is_empty(),
            "`evict`'s transient unit IS the always-on backend's declared unit: {got:?}"
        );

        // CONTROL: rename the on-demand backend so nothing collides, and it is
        // stoppable again — the rule must key on the collision, not on the
        // presence of an always-on entry.
        let ok = r#"{"backends":{
            "pinned":{"url":"http://x","kind":"llama-server","hardware":"gpu",
                      "unit":"chord-evict.service","always_on":true},
            "other":{"url":"http://y","kind":"llama-server","hardware":"gpu",
                     "always_on":false}}}"#;
        let got = stoppable_gpu_backends_from_json(ok, "llama-gpu");
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].name(), "other");
    }

    #[test]
    fn protected_units_covers_declared_and_transient() {
        let raw = r#"{"backends":{
            "ollama":{"url":"http://x","kind":"ollama","hardware":"gpu",
                      "unit":"ollama.service","always_on":true},
            "lemonade":{"url":"http://y","kind":"llama-server","hardware":"gpu",
                        "unit":"lemonade-coder.service","always_on":false}}}"#;
        let p = protected_units_from_json(raw);
        assert!(p.contains("ollama.service"), "declared unit: {p:?}");
        assert!(p.contains("chord-ollama.service"), "transient unit: {p:?}");
        assert!(
            !p.contains("lemonade-coder.service") && !p.contains("chord-lemonade.service"),
            "an on-demand backend's units are NOT protected — it must stay \
             evictable: {p:?}"
        );
    }

    #[test]
    fn transient_unit_names_the_chord_scoped_unit() {
        assert_eq!(transient_unit("llama-gpu"), "chord-llama-gpu.service");
    }

    #[test]
    fn unmanaged_kind_is_the_ollama_rule() {
        assert!(is_unmanaged_kind(Some("ollama")));
        assert!(!is_unmanaged_kind(Some("llama-server")));
        assert!(!is_unmanaged_kind(Some("daemon")));
        assert!(!is_unmanaged_kind(None));
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
    ///
    /// `ensure_up`'s argv is deliberately split across lines: the first version of
    /// this scanner was line-based and could not see it at all.
    const SAMPLE: &str = r#"
/// docs mentioning fn free_gpu in prose
pub fn stop(b: &B) {
    // mirrors fn reap_all in the pre-guard code
    let hook: fn (u8) -> u8 = |x| x;
    let _ = run(["systemctl", "stop", unit]);
}

fn helper<F: Fn(u8) -> u8>(f: F) -> u8 { f(1) }

pub async fn ensure_up(b: &B) {
    let _ = run([
        "systemctl",
        "stop",
        &unit_name,
    ]);
}
"#;

    #[test]
    fn ratchet_attributes_stop_sites_to_their_enclosing_fn() {
        assert_eq!(
            stop_call_site_owners(SAMPLE),
            vec!["stop".to_string(), "ensure_up".to_string()]
        );
    }

    /// The ratchet's whole job: a NEW function that stops a unit must show up —
    /// in EVERY form a Rust function can legally take. Round-1 review found the
    /// scanner blind to all of these, which meant the ratchet would have gone on
    /// reporting "no new stop sites" while one sat in the file.
    #[test]
    fn ratchet_catches_a_new_unguarded_stop_site_in_any_function_form() {
        let forms: [(&str, &str); 6] = [
            ("pub(crate) fn rogue_a()", "rogue_a"),
            ("const fn rogue_b()", "rogue_b"),
            ("unsafe fn rogue_c()", "rogue_c"),
            ("pub(super) async fn rogue_d()", "rogue_d"),
            ("#[inline] pub(crate) fn rogue_e()", "rogue_e"),
            ("fn rogue_f()", "rogue_f"),
        ];
        for (header, name) in forms {
            let rogue = format!(
                "{SAMPLE}\n{header} {{\n    let _ = run([\"systemctl\", \"stop\", name]);\n}}\n"
            );
            let owners = stop_call_site_owners(&rogue);
            assert!(
                owners.contains(&name.to_string()),
                "`{header}` must be attributed as a new stop site, not absorbed into \
                 an allow-listed function; got {owners:?}"
            );
        }
    }

    /// Round-2 review (codex, gpt56) found these: valid Rust with a COMMENT
    /// between the modifiers and the name, which the previous scanner could not
    /// see — so the stop inside was attributed to the previous, allow-listed
    /// function and the ratchet stayed green.
    #[test]
    fn ratchet_sees_through_comments_in_a_function_header() {
        for header in [
            "pub /* ordinary comment */ fn rogue()",
            "fn /* c */ rogue()",
            "pub(crate) /* why */ fn rogue()",
        ] {
            let src = format!(
                "{SAMPLE}\n{header} {{\n    let _ = run([\"systemctl\", \"stop\", u]);\n}}\n"
            );
            let owners = stop_call_site_owners(&src);
            assert!(
                owners.contains(&"rogue".to_string()),
                "`{header}` must be attributed, not absorbed into an allow-listed \
                 function; got {owners:?}"
            );
        }
    }

    /// Comment stripping must not eat a `//` that lives inside a STRING — the
    /// classic way a naive comment stripper corrupts a file full of URLs.
    #[test]
    fn comment_stripping_respects_string_literals() {
        // The `//` lives inside a STRING, on the SAME LINE as the stop site and
        // BEFORE it. A stripper without string tracking treats it as a comment
        // opener and deletes the rest of the line — losing a real stop site
        // entirely. That is the direction that matters: a ratchet that drops sites
        // is worse than no ratchet.
        let src = "fn f() {\n    let u = \"http://host/x\"; let _ = run([\"systemctl\", \"stop\", u]);\n}\n";
        assert_eq!(stop_call_site_owners(src), vec!["f".to_string()]);
        assert_eq!(systemctl_literal_count(src), 1);
    }

    /// The census is the half that syntax cannot evade. Every bypass round 2
    /// raised — a comment in the header, a non-literal verb — still has to name
    /// `systemctl`, so the count moves.
    #[test]
    fn the_census_counts_every_systemctl_literal_however_the_call_is_spelled() {
        let base = "fn a() {\n    let _ = run([\"systemctl\", \"stop\", u]);\n}\n";
        assert_eq!(systemctl_literal_count(base), 1);

        // The exact evasion codex named: the verb is not a literal, so the
        // attribution scan cannot see it — but the census can.
        let non_literal = format!(
            "{base}pub /* hide me */ fn b() {{\n    let verb = \"stop\";\n    let _ = run([\"systemctl\", verb, u]);\n}}\n"
        );
        assert!(
            stop_call_site_owners(&non_literal).iter().all(|o| o != "b"),
            "documented limit: a non-literal verb is invisible to the ATTRIBUTION half"
        );
        assert_eq!(
            systemctl_literal_count(&non_literal),
            2,
            "...which is exactly why the CENSUS half exists"
        );

        // A `systemctl` mention in a comment is not an invocation and must not
        // inflate the census.
        let commented = format!("{base}// see run([\"systemctl\", \"stop\", u]) above\n");
        assert_eq!(systemctl_literal_count(&commented), 1);
    }

    /// A stop site written with different spacing must still be seen — the
    /// normalisation, not the exact literal, is what the scan depends on.
    #[test]
    fn ratchet_is_insensitive_to_argv_formatting() {
        for argv in [
            "[\"systemctl\",\"stop\", u]",
            "[\"systemctl\",    \"stop\", u]",
            "[\n    \"systemctl\",\n    \"stop\",\n    u,\n]",
        ] {
            let src = format!("fn rogue() {{\n    let _ = run({argv});\n}}\n");
            assert_eq!(
                stop_call_site_owners(&src),
                vec!["rogue".to_string()],
                "formatting must not hide a stop site: {argv}"
            );
        }
    }

    /// The prefix rule accepts every legal item-fn modifier and rejects the
    /// near-misses. Tested directly because it is the part that was wrong.
    #[test]
    fn item_fn_prefix_accepts_modifiers_and_rejects_prose() {
        for ok in [
            "", "pub", "async", "pub async", "pub(crate)", "pub(super)",
            "pub(in crate::intake)", "const", "unsafe", "pub unsafe",
            "extern \"C\"", "#[inline]", "#[inline] pub(crate)",
        ] {
            assert!(is_item_fn_prefix(ok), "should accept prefix {ok:?}");
        }
        for bad in ["///", "// mirrors", "let hook:", "F: Fn(u8) ->", "type T ="] {
            assert!(!is_item_fn_prefix(bad), "should reject prefix {bad:?}");
        }
    }

    #[test]
    fn ratchet_reports_nothing_when_there_is_nothing() {
        assert!(stop_call_site_owners("fn quiet() { let _ = 1; }").is_empty());
    }
}
