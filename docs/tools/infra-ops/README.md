# Infra & Ops tools

[← docs index](../../README.md) · [← tool index](../README.md)

The Infra & Ops domain is fleet health, automation, secrets, networking, and
admin surfaces — 14 modules registering roughly 76 tools between them. Three
modules (`ansible`, `<secret-manager>`, `approval`) sit behind the shared
per-occurrence [approval gate](../../../src/approval.rs); the rest are
read-only HTTP probes against LAN services or fixed-command SSH bridges into
the fleet host.

<img src="../../../assets/infra-ops-overview.svg" alt="Infra & Ops domain map: guarded, read-only HTTP, and fleet-host SSH bridge module groups" width="100%">

## Modules

| Module | Tools | What it does | Page |
| --- | --- | --- | --- |
| `ansible` | 4 | GUARDED. Runs allowlisted Ansible playbooks on the ansible control host over typed SSH, lists playbooks, and reports the last run's status/log. | [`ansible.md`](ansible.md) |
| `<secret-manager>` | 5 | GUARDED. Read-only secret queries against <secret-manager> (Universal Auth) — status, list projects, list secret keys, get one secret, get a batch. | [`<secret-manager>.md`](<secret-manager>.md) |
| `approval` | 2 | GUARDED. `approval_grant`/`approval_deny` — the operator-only handlers that flip a pending guarded-tool request; the model can never call these itself. | [`approval.md`](approval.md) |
| `prometheus` | 6 | Read-only PromQL queries, targets, alerts, and a pre-built cluster health summary against a LAN Prometheus server. | [`prometheus.md`](prometheus.md) |
| `<container-mgr>` | 4 | Read-only Docker container/environment/log queries via the <container-mgr> API (self-signed TLS accepted). | [`<container-mgr>.md`](<container-mgr>.md) |
| `network` | 5 | Network diagnostics — SSH-based ping and subnet sweep, pure-Rust TCP port check, DNS lookup, and named-service reachability. | [`network.md`](network.md) |
| `dura` | 7 | Sysadmin/health-check tools over typed SSH and Prometheus — smoke test, backup listing, journal log query, constellation health, running services, disk usage, per-service check. | [`dura.md`](dura.md) |
| `soma` | 10 | The Lumina Constellation admin panel/API — status (no auth), and nine `X-Soma-Key`-authenticated reads/writes (config, inference status, cost, backups, validation, skills, agent rename). | [`soma.md`](soma.md) |
| `synapse` | 3 | Proactive-message ("Pulse") gate control — status (local config/log read), manual trigger, and mute. | [`synapse.md`](synapse.md) |
| `sentinel` | 3 | Operational checks and logging on the fleet host, plus a live MooseNet status page refresh. | [`sentinel.md`](sentinel.md) |
| `vigil` | 2 | Morning/afternoon briefing generation and status polling. | [`vigil.md`](vigil.md) |
| `sysversion` | 1 | `system_version` — a single never-fail tool reporting version/reachability of every constellation component. | [`sysversion.md`](sysversion.md) |
| `gateway` (dashboard) | 6 | Surfaces the Lumina API Gateway / Homepage dashboard — health, calendar, tasks, insights, inbox, and a forced composer refresh. | [`dashboard.md`](dashboard.md) |
| `sundry` | 6 | Small one-off utility tools: `health`, `echo`, `utc_now`, `constellation_version`, `vector_onboard`, `searxng_search`. | [`sundry.md`](sundry.md) |

## Cross-cutting patterns

**Guarded tools.** `ansible`, `<secret-manager>`, and `approval` route every
`execute()` through [`approval::gate()`](../../../src/approval.rs) before any
real work happens. A call with no `_approval_code` creates a pending row in
`tool_approvals` (Postgres, `DATABASE_URL`) and returns an "APPROVAL
REQUIRED" message instead of running; the operator approves out of band in
chat, and lumina-core's deterministic (non-LLM) handler re-dispatches the
stored call. See [`ansible.md`](ansible.md#the-approval-gate-in-detail) for
the full sequence.

**No hardcoded infrastructure.** Every module reads its host, port, key
path, and credential from an environment variable — never a compiled-in
default that encodes a real internal address. Several modules (`ansible`,
`synapse`, `sentinel`, `vigil`, `gateway`) went through a 2026-07 PII
remediation pass that removed compiled-in fallback strings (allowlists,
script paths, status-generator commands) that had previously embedded real
fleet paths; a missing env var now fails clean with `NotConfigured` instead
of silently using a guessed value.

**SSH is typed, never shelled.** Every SSH-based tool in this domain uses
the `ssh2` crate for a typed session/channel — no `std::process::Command`,
no `shell=True` equivalent. Commands are either fully fixed strings (no user
input at all) or built only from validated tokens (allowlisted names,
numeric ranges, alphanumeric-only path segments).

**Error genericization.** Connection-level SSH failures (unreachable host,
handshake failure, auth failure) are rewritten into generic messages before
reaching the caller — they never leak the internal hostname, IP, or port
number. `synapse` is the one deliberate exception: its live Python
predecessor returned raw `ssh:`-prefixed stderr as normal tool output, so
this Rust port preserves that exact contract instead of genericizing it (see
[`synapse.md`](synapse.md)).

[← docs index](../../README.md) · [← tool index](../README.md)
