# Terminus Documentation

Terminus is the Lumina Constellation's MCP tool hub: 381 core tools served through
one authenticated gateway (`terminus_primary`), a personal/admin deployment
(`terminus_personal`), and the engines — model intake, Atlas knowledge graph,
review, docgen, mirror, compiler — that keep the fleet self-maintaining. Start at
the [README](../README.md) for the landing overview.

## Core pages

| Page | Description |
|---|---|
| [Getting Started](getting-started.md) | Clone, build, configure (env-key names only), run `terminus_primary`, and connect an MCP client end to end. |
| [Architecture](architecture.md) | The full subsystem diagram derived from the code knowledge graph, one narrative paragraph per subsystem, and the life of a tool call. |
| [Reference index](reference/index.md) | Inventory of all 17 KG-derived subsystems with links to the 13 deep reference pages. |
| [Guides index](guides/index.md) | Task-oriented operator guides grounded in real binaries and tools. |

## Subsystem reference

| Page | Description |
|---|---|
| [intake](reference/intake.md) | Model profiling framework: context/coder/assistant suites, timeouts, GPU authority, discovery, fleet jobs. |
| [forge](reference/forge.md) | Provider-agnostic forge abstraction, the Gitea-family/GitLab adapters, and the PII-swept git-public mirror engine. |
| [tools](reference/tools.md) | The docgen documentation engine (`docgen_*`) and the serving control/status tools. |
| [scribe](reference/scribe.md) | The Atlas per-project code knowledge graph (`kg_*`) and the standing documentation agent (`scribe_*`). |
| [plane](reference/plane.md) | 43 Plane CE tools: multi-identity PATs, proactive rate pacing, optional shared Redis cache. |
| [mesh](reference/mesh.md) | Upstream federation registry, `Principal` caller identity, optional embedded tailnet listener. |
| [cortex](reference/cortex.md) | Atlas-backed blast-radius (`cortex_scope`), risk scoring (`cortex_review`), audits, and calibration. |
| [media](reference/media.md) | Typed clients for Radarr/Sonarr/Prowlarr/qtor/Plex/<media-service>/TMDb and the media tool surface. |
| [gitea](reference/gitea.md) | Gitea REST tool suite, named-identity `GITEA_PAT_<NAME>` model, and the merge queue. |
| [review](reference/review.md) | `review_run` multi-provider review: structures, verdict parsing, KG grounding, provider capacity. |
| [github](reference/github.md) | GitHub org tools plus the authoritative PII scan/redact engine used by every write gate. |
| [broker](reference/broker.md) | Out-of-process worker routing: route table, transport tiers, admin control plane, blue-green rollout. |
| [pg](reference/pg.md) | The identity-scoped, approval-gated Postgres tool suite. |

## Operator guides

| Page | Description |
|---|---|
| [Run a model-intake sweep](guides/run-a-model-intake-sweep.md) | Profile fleet models with the `mint` CLI (coder/assistant sweeps, single cases, GPU authority). |
| [Run a review panel](guides/run-a-review-panel.md) | Set up `review_daemon` and dispatch a multi-provider `review_run` panel. |
| [Run the git-public mirror](guides/run-the-git-public-mirror.md) | Produce a PII-swept public mirror pass with `git_public_mirror_run` and the `pii_gate` binary. |

## Existing deep dives

Long-form pages that predate this docs tree and remain authoritative for their topics:

- [Per-tool documentation](tools/README.md) — grouped by domain (code-git, mint, models-review, infra-ops, project-planning, personal-life, [postgres-suite](tools/postgres-suite.md)).
- Architecture deep dives: [auth](architecture/auth.md) · [broker](architecture/broker.md) · [chord-integration](architecture/chord-integration.md) · [federation](architecture/federation.md) · [mesh](architecture/mesh.md).
- Build and quality: [build.md](build.md) · [house-style.md](house-style.md) · [cortex-elegance-gate.md](cortex-elegance-gate.md) · [cortex-calibration.md](cortex-calibration.md).
- Earlier generated reference pages (kept for their per-feature detail): [compiler_build (BLD-05)](reference/compiler-build-the-single-build-door-bld-05.md), [compiler_request (BLD-06)](reference/compiler-request-queue-scheduler-bld-06.md), [compiler_status (BLD-08)](reference/compiler-status-fleet-version-query-bld-08.md), [compiler_deploy (BLD-13)](reference/compiler-deploy-trigger-on-publish-fleet-wide-bld-13.md), [compiler_progress (BLD-19)](reference/compiler-progress-live-build-progress-events-bld-19.md), [fleet clock (CLK-01)](reference/fleet-clock-time-now-clk-01.md), [mesh federation](reference/mesh-federating-multiple-upstream-terminus-servers.md), [Principal identity (MESH-06)](reference/unified-principal-identity-mesh-06.md), [constellation aggregation API (CONST-02)](reference/constellation-aggregation-api-const-02.md), [Atlas KG query tools](reference/atlas-knowledge-graph-query-tools.md), [cortex risk gate (CXEG)](reference/cortex-code-elegance-risk-gate-atlas-backed-s115-cxeg.md), [Postgres suite (S115)](reference/postgres-tool-suite-the-single-sanctioned-postgres-door-s115.md), [Redis backend (BLD-20)](reference/redis-backend-the-shared-fleet-cache-queue-limiter-bld-20.md), [MINT idle-mode (BLD-10)](reference/mint-idle-mode-release-the-host-for-a-ci-cd-compiler-run-bld-10.md).
