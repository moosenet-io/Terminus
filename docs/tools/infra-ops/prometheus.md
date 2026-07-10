# prometheus

[← Infra & Ops index](README.md) · [← tool index](../README.md)

Source: [`src/prometheus/mod.rs`](../../../src/prometheus/mod.rs)

The `prometheus` module is a read-only PromQL client against a LAN
Prometheus server. It mirrors a legacy Python `prometheus_tools.py` exactly
(`src/prometheus/mod.rs:1-9`) and six tools cover status, ad hoc instant and
range queries, target listing, alert listing, and a pre-built cluster health
summary. Prometheus runs LAN-only without authentication in this
deployment, so the module sends no credentials at all
(`src/prometheus/mod.rs:14-15`).

<img src="../../../assets/readonly-http-probe-flow.svg" alt="Read-only HTTP probe flow: GET the LAN service, shape the response, return JSON or a genericized error" width="100%">

## Configuration

| Env var | Purpose |
| --- | --- |
| `PROMETHEUS_URL` | Base URL of the Prometheus server, e.g. `http://prometheus.example:9090` |

If unset, `register()` installs `NotConfiguredStub` tools under all six
names instead of failing startup — every call then returns
`ToolError::NotConfigured("PROMETHEUS_URL not set")` immediately
(`src/prometheus/mod.rs:631-654,657-667`). Same pattern used by `<container-mgr>`.

## Tools

### `prometheus_status`

**Purpose.** Health check plus a target summary.

**Input schema.** No parameters.

**Behavior.** GET `{base}/-/healthy` (any 2xx = healthy; any failure =
`healthy: false`, never an error), then GET `/api/v1/targets` and count
`up`/`down` from the active-targets array.

**Output shape:**
```json
{"healthy": true, "url": "http://prometheus.example:9090", "targets_total": 12, "targets_up": 11, "targets_down": 1}
```

### `prometheus_query`

**Purpose.** Run an arbitrary PromQL instant query (`/api/v1/query`).

**Input schema** (`src/prometheus/mod.rs:279-287`)

| Field | Type | Required | Notes |
| --- | --- | --- | --- |
| `query` | string | yes | PromQL expression, e.g. `up`, `node_load1` |

**Behavior.** Empty/missing `query` → `InvalidArgument`. Otherwise GETs
`/api/v1/query?query=<query>` and reshapes via `format_instant_query`
(`src/prometheus/mod.rs:127-182`), which branches on Prometheus's own
`resultType`:

| `resultType` | Extracted shape per result entry |
| --- | --- |
| `vector` | `{"labels", "timestamp", "value"}` |
| `matrix` | `{"labels", "values": [{"t", "v"}, ...]}` |
| `scalar` | `{"labels", "value"}` |
| anything else | `{"labels"}` only — no crash on an unrecognized type |

**Output shape:**
```json
{"query": "up", "result_type": "vector", "count": 2, "results": [
  {"labels": {"__name__": "up", "instance": "node1", "job": "node"}, "timestamp": 1717000000.0, "value": "1"},
  {"labels": {"instance": "node2", "job": "node"}, "timestamp": 1717000000.0, "value": "0"}
]}
```

### `prometheus_query_range`

**Purpose.** Run a PromQL range query over a time window (`/api/v1/query_range`).

**Input schema** (`src/prometheus/mod.rs:313-323`)

| Field | Type | Required | Default | Notes |
| --- | --- | --- | --- | --- |
| `query` | string | yes | — | PromQL expression |
| `duration` | string | no | `"1h"` | How far back — `parse_duration_secs` accepts `Ns`/`Nm`/`Nh`/`Nd`; unknown unit or unparseable number silently defaults to 3600s (`src/prometheus/mod.rs:96-114`) |
| `step` | string | no | `"60s"` | Resolution step, passed through to Prometheus verbatim |

**Behavior.** `start = now - duration_secs`, `end = now`. Reshapes each
series into `{"labels", "datapoints": <count>, "values": [{"t","v"}, ...]}`.

**Output shape:**
```json
{"query": "node_load1", "duration": "6h", "step": "60s", "series_count": 3, "results": [
  {"labels": {"instance": "n1"}, "datapoints": 360, "values": [{"t": 100.0, "v": "0.5"}, ...]}
]}
```

### `prometheus_targets`

**Purpose.** List every scrape target and its health, down targets first.

**Input schema.** No parameters.

**Behavior.** GETs `/api/v1/targets`; each entry maps to
`{"job", "instance", "health", "last_scrape", "last_error"}` (unknown
fields default to `"unknown"`/`""`). Sorted `(!up, job, instance)` so down
targets surface first, then alphabetically.

**Output shape:**
```json
{"total": 12, "up": 11, "down": 1, "targets": [{"job": "chord", "instance": "node-a:9100", "health": "down", "last_scrape": "...", "last_error": "..."}, ...]}
```

### `prometheus_alerts`

**Purpose.** List firing/pending alerts.

**Input schema.** No parameters.

**Behavior.** GETs `/api/v1/alerts`; maps each alert to
`{"name", "state", "severity", "instance", "summary", "active_since"}`
pulled from `labels`/`annotations`, defaulting `severity` to `"none"` when
absent. Counts `firing`/`pending` separately.

**Output shape:**
```json
{"total": 2, "firing": 1, "pending": 1, "alerts": [{"name": "DiskFull", "state": "firing", "severity": "critical", "instance": "ct315", "summary": "...", "active_since": "..."}]}
```

Requires alerting rules to actually be configured in Prometheus — an empty
list is a valid, non-error response.

### `prometheus_health_summary`

**Purpose.** A single pre-built cluster health dashboard — the most
"compound" tool in this module, issuing seven separate PromQL queries and
merging them per-instance.

**Input schema.** No parameters.

**Behavior.** Sequentially queries `node_load1`,
`node_memory_MemTotal_bytes`, `node_memory_MemAvailable_bytes` (used to
derive `mem_used_pct`), `node_filesystem_size_bytes{mountpoint="/"}` and
`node_filesystem_avail_bytes{mountpoint="/"}` (derives
`disk_root_used_pct`/`disk_root_avail_gb`), plus `/api/v1/targets` and
`/api/v1/alerts` for cluster-wide counts. All per-instance values are
rounded to 1-2 decimal places before being merged into one `nodes` map
keyed by instance label.

**Output shape:**
```json
{
  "nodes": {
    "node-a": {"load_1m": 2.31, "mem_total_gb": 31.0, "mem_available_gb": 12.4, "mem_used_pct": 60.0,
             "disk_root_used_pct": 41.2, "disk_root_avail_gb": 220.5}
  },
  "cluster": {"targets_total": 12, "targets_up": 11, "targets_down": 1, "alerts_firing": 1}
}
```

**Errors / edge cases (all six tools).** Any HTTP-layer failure (unreachable,
non-2xx, bad JSON) surfaces as `ToolError::Http` with a genericized message
("The metrics service (Prometheus) is unreachable." — `src/prometheus/mod.rs:70-79`)
that omits the actual URL/host; the raw error is only logged via `warn!`,
never returned to the caller.

## Security model summary

- Fully read-only — no write path anywhere in this module.
- No credentials sent — Prometheus is LAN-trusted, not identity-authenticated.
- Missing config degrades to explicit `NotConfigured` stubs rather than a
  panic or a silently-wrong default URL.

[← Infra & Ops index](README.md) · [← tool index](../README.md)
