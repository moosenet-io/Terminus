//! PROMEX-01: application-level Prometheus metrics exporter.
//!
//! A process-global [`prometheus::Registry`] plus a small, fixed set of
//! application metrics — tool-call volume and latency — exposed as
//! `GET /metrics` in the standard Prometheus text exposition format (see
//! `crate::mcp_server::handle_metrics`).
//!
//! ## Design
//! - **One registry, lazily built once per process** (`OnceLock`), matching
//!   the existing process-wide-cache idiom used elsewhere in this crate
//!   (e.g. `crate::sysversion`'s `CACHE: OnceLock<..>`) rather than pulling
//!   in a separate `lazy_static`/`once_cell` dependency.
//! - **Two metrics, deliberately minimal** (this item is a REFERENCE PATTERN
//!   meant to be replicated to other services, so it stays small and
//!   readable rather than trying to cover every possible signal up front):
//!   - `terminus_tool_calls_total{tool, result}` — a `CounterVec`, `result`
//!     is always `"ok"` or `"error"` (never a raw error message or any other
//!     caller-controlled value, so cardinality stays bounded by tool count).
//!   - `terminus_tool_duration_seconds{tool}` — a `HistogramVec` of
//!     end-to-end dispatch latency, default bucket boundaries.
//! - **No secrets, no caller-controlled label values.** The `tool` label is
//!   passed through [`bounded_tool_label`] at the call site, which maps a
//!   caller-supplied `tools/call` name onto a BOUNDED set — {known local tool
//!   names} ∪ {upstream namespaces as `<mesh:ns>`} ∪ {`<unknown>`} — so an
//!   arbitrary or unknown name (including a mesh `<ns>__<caller-chosen>` name,
//!   which `resolve_call_route` does NOT validate against the upstream
//!   catalog) can never inflate cardinality or leak a secret-shaped string
//!   into a label. `result` is likewise a closed `"ok"`/`"error"` set.
//! - **Read-only, unauthenticated, always-on.** This crate's existing
//!   `/healthz` route is likewise unauthenticated (see `mcp_server`'s module
//!   doc, "Auth" section) — metrics are equally non-sensitive (counts and
//!   timings only), so there is no `TERMINUS_METRICS_*` env gate to check;
//!   the endpoint is simply mounted alongside `/healthz`.
//!
//! ## Usage
//! Call [`record_tool_call`] exactly once per dispatched tool call from the
//! single central dispatch point in `crate::mcp_server::handle_mcp`'s
//! `tools/call` branch (see that module for exactly where). Call
//! [`gather_text`] from the `/metrics` HTTP handler.

use std::borrow::Cow;
use std::sync::OnceLock;
use std::time::Duration;

use prometheus::{CounterVec, HistogramVec, Registry, TextEncoder};

/// The result label recorded on `terminus_tool_calls_total`. Deliberately a
/// closed two-value set (never the raw error message) so the metric's
/// cardinality is bounded by `tool count * 2`, not by arbitrary error text.
const RESULT_OK: &str = "ok";
const RESULT_ERROR: &str = "error";

struct Metrics {
    registry: Registry,
    tool_calls_total: CounterVec,
    tool_duration_seconds: HistogramVec,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

fn metrics() -> &'static Metrics {
    METRICS.get_or_init(|| {
        let registry = Registry::new();

        let tool_calls_total = CounterVec::new(
            prometheus::Opts::new(
                "terminus_tool_calls_total",
                "Total number of terminus tool dispatches, by tool name and outcome.",
            ),
            &["tool", "result"],
        )
        .expect("terminus_tool_calls_total: static metric definition is well-formed");

        let tool_duration_seconds = HistogramVec::new(
            prometheus::HistogramOpts::new(
                "terminus_tool_duration_seconds",
                "Tool dispatch latency in seconds, by tool name.",
            ),
            &["tool"],
        )
        .expect("terminus_tool_duration_seconds: static metric definition is well-formed");

        registry
            .register(Box::new(tool_calls_total.clone()))
            .expect("terminus_tool_calls_total: single registration at process startup");
        registry
            .register(Box::new(tool_duration_seconds.clone()))
            .expect("terminus_tool_duration_seconds: single registration at process startup");

        Metrics {
            registry,
            tool_calls_total,
            tool_duration_seconds,
        }
    })
}

/// Record one completed tool dispatch: increments
/// `terminus_tool_calls_total{tool, result}` and observes `duration` into
/// `terminus_tool_duration_seconds{tool}`.
///
/// `tool` should be the bare dispatched tool name (not a namespaced
/// `<mesh-namespace>__<tool>` name, and not raw request arguments) — see
/// this module's doc for why label values must come from a bounded set.
pub fn record_tool_call(tool: &str, is_ok: bool, duration: Duration) {
    let m = metrics();
    let result = if is_ok { RESULT_OK } else { RESULT_ERROR };
    m.tool_calls_total.with_label_values(&[tool, result]).inc();
    m.tool_duration_seconds
        .with_label_values(&[tool])
        .observe(duration.as_secs_f64());
}

/// Map a caller-supplied `tools/call` name onto a BOUNDED metric label value,
/// so the `tool` label can never be inflated by an arbitrary or unknown name.
///
/// The two bounding facts come from the CALLER (kept out of this fn so it stays
/// pure/testable), and BOTH are derived from validated state, never from the
/// raw request string:
/// * `is_known_local` — whether the active registry snapshot contains `name`
///   as a local tool (`reg.contains(name)`).
/// * `configured_mesh_ns` — `Some(ns)` ONLY when `resolve_call_route` resolved
///   the call to a genuinely CONFIGURED upstream (an `Upstream`/`Unavailable`
///   route), i.e. `ns` is a real, bounded upstream namespace — NOT merely a
///   `foo__bar`-shaped string (an unknown prefix resolves to `Local`, giving
///   `None` here, so a caller cannot smuggle an arbitrary/secret-shaped prefix
///   into the label).
///
/// Mapping → bounded set {known local names} ∪ {`<mesh:ns>` for configured ns}
/// ∪ {`<unknown>`}:
pub fn bounded_tool_label<'a>(
    name: &'a str,
    is_known_local: bool,
    configured_mesh_ns: Option<&str>,
) -> Cow<'a, str> {
    if is_known_local {
        Cow::Borrowed(name)
    } else if let Some(ns) = configured_mesh_ns {
        Cow::Owned(format!("<mesh:{ns}>"))
    } else {
        Cow::Borrowed("<unknown>")
    }
}

/// Encode every registered metric in the Prometheus text exposition format
/// (the `GET /metrics` response body).
pub fn gather_text() -> String {
    let m = metrics();
    let families = m.registry.gather();
    let encoder = TextEncoder::new();
    encoder
        .encode_to_string(&families)
        .unwrap_or_else(|e| format!("# error encoding metrics: {e}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_tool_call_appears_in_gathered_text() {
        record_tool_call("promex01_test_tool", true, Duration::from_millis(42));

        let text = gather_text();
        assert!(
            text.contains("terminus_tool_calls_total"),
            "expected counter family name in output:\n{text}"
        );
        assert!(
            text.contains("terminus_tool_duration_seconds"),
            "expected histogram family name in output:\n{text}"
        );
        assert!(
            text.contains("tool=\"promex01_test_tool\""),
            "expected the recorded tool label in output:\n{text}"
        );
        assert!(
            text.contains("result=\"ok\""),
            "expected the ok result label in output:\n{text}"
        );
    }

    #[test]
    fn bounded_tool_label_known_local_passes_through() {
        assert_eq!(bounded_tool_label("pg_query", true, None), "pg_query");
    }

    #[test]
    fn bounded_tool_label_unknown_is_sentinel_even_when_underscore_shaped() {
        // A name that isn't a known local tool and has NO configured upstream
        // namespace collapses to the fixed sentinel — including a `foo__bar`
        // shaped name whose prefix is NOT a real upstream (resolve_call_route
        // gives it a Local route, so the caller passes configured_mesh_ns=None).
        // This is the key guard: an arbitrary/secret-shaped prefix cannot reach
        // the label.
        assert_eq!(bounded_tool_label("totally_made_up_xyz", false, None), "<unknown>");
        assert_eq!(bounded_tool_label("customer_secret__anything", false, None), "<unknown>");
    }

    #[test]
    fn bounded_tool_label_configured_mesh_ns_buckets_by_namespace() {
        // Only when the caller confirms a CONFIGURED upstream namespace do we
        // bucket by `<mesh:ns>` — bounded by the configured upstream set.
        assert_eq!(bounded_tool_label("pve__vm_list", false, Some("<host>")), "<mesh:<host>>");
        assert_eq!(bounded_tool_label("pve__anything", false, Some("<host>")), "<mesh:<host>>");
    }

    #[test]
    fn record_tool_call_error_uses_error_result_label() {
        record_tool_call("promex01_test_tool_err", false, Duration::from_millis(1));

        let text = gather_text();
        assert!(
            text.contains("tool=\"promex01_test_tool_err\",result=\"error\"")
                || text.contains("result=\"error\",tool=\"promex01_test_tool_err\""),
            "expected an error-result sample for the tool in output:\n{text}"
        );
    }
}
