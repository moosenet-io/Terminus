# Subsystem Reference

Subsystems are derived from the code knowledge graph by top-level module path
(analyzed at `3d0f277`; 11,905 nodes total). Node counts below are KG symbol
counts. Thirteen subsystems have dedicated deep pages; the remainder are
inventoried at the bottom.

| Subsystem | Symbols | Source | Page |
|---|---|---|---|
| intake | 2,059 | `src/intake` | [intake.md](intake.md) |
| forge | 836 | `src/forge` | [forge.md](forge.md) |
| tools | 772 | `src/tools` | [tools.md](tools.md) |
| scribe | 739 | `src/scribe` | [scribe.md](scribe.md) |
| plane | 514 | `src/plane` | [plane.md](plane.md) |
| cortex | 424 | `src/cortex` | [cortex.md](cortex.md) |
| media | 424 | `src/media` | [media.md](media.md) |
| gitea | 292 | `src/gitea` | [gitea.md](gitea.md) |
| bin | 271 | `src/bin` | covered in [architecture](../architecture.md) |
| review | 263 | `src/review` | [review.md](review.md) |
| mesh | 258 | `src/mesh` | [mesh.md](mesh.md) |
| github | 255 | `src/github` | [github.md](github.md) |
| pg | 219 | `src/pg` | [pg.md](pg.md) |
| broker | 213 | `src/broker` | [broker.md](broker.md) |
| constellation-web | 366 | `constellation-web/src` | no page yet — TS control-plane UI (aggregation client, module registry, status hooks) |
| compat | 161 | `src/compat` | no page yet — vendored conversation-buffer/prompt types (`ConversationBuffer` is a top-5 KG hotspot) |
| misc (crate root) | 3,839 | `src/*.rs`, single-integration modules | see below |

## Crate-root and single-integration modules (the "misc" 3,839)

The registry (`src/registry.rs`), MCP server (`src/mcp_server.rs`), config
(`src/config.rs`), error/tool contracts (`src/error.rs`, `src/tool.rs`), plus
one module per integrated service: `ansible`, `approval` (the per-occurrence
tool-approval gate), `axon`, `commute`, `compiler` (the `compiler_*` CI/CD build
door — see the BLD pages linked from the [docs index](../index.md)), `constellation`
(aggregation API), `council`, `crucible`, `dev`, `dgem`, `dura`, `federation`,
`gateway`, `gateway_framework`, `google`, `hearth`, `house_style`,
`inference_proxy`, `<secret-manager>`, `<media-service>`, `ledger`, `litellm`, `lumina_ext`,
`meridian`, `metrics`, `mint` (idle mode), `model_advisor`, `myelin`, `network`,
`news`, `nexus`, `odyssey`, `openhands`, `pki`, `<container-mgr>`, `prometheus`,
`ratelimit`, `redis`, `relay`, `reminder`, `routines`, `secrets_bootstrap`,
`seer`, `sentinel`, `skills`, `soma`, `sundry`, `synapse`, `sysversion`, `time`,
`vector`, `vigil`, `vitals`, `weather`, `wizard`. Most register a handful of
tools each; per-tool documentation lives under [docs/tools/](../tools/README.md).

Workspace members (not part of the KG rollup above): `terminus-client`
(enrollment + mTLS transport + local forwarding daemon) and
`terminus-worker-sdk` (worker authoring surface for [broker](broker.md) workers).
