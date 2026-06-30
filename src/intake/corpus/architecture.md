# Architecture

Lumina Constellation is a self-hosted personal AI assistant built as a Rust workspace. This
document describes the system design, how a request flows through it, the memory model, the
model-management strategy, and the design principles that hold it together.

## System overview

The system is three cooperating Rust binaries plus a local model server:

```
                         You (chat channel)
                                |
                                v
   +----------------------------------------------------------+
   |                       lumina-core                         |
   |  chat I/O . persona + prompt assembly . scheduler .       |
   |  three-tier memory subsystem . session + security        |
   +----------------------------+-----------------------------+
                                |
                                v
   +----------------------------------------------------------+
   |                       chord-proxy                         |
   |  request classification . cost-tier routing .            |
   |  agentic tool-calling loop . model lifecycle             |
   |  (hot / warm / cold) . control API                       |
   +----------------+----------------------------+------------+
                    |                            |
        +-----------v-----------+    +-----------v------------+
        |      terminus-rs      |    |   inference backends   |
        |   MCP tool hub        |    |   local models         |
        |   (100+ tools)        |    |   + optional cloud     |
        +-----------------------+    +------------------------+
```

| Crate | Role |
|-------|------|
| `lumina-core` | Orchestrator: owns chat channels, assembles the persona/system prompt, runs the scheduler, manages memory and sessions, enforces security/egress policy. |
| `chord-proxy` | Smart inference proxy: classifies requests, routes across cost tiers, runs the agentic tool-calling loop, and manages the model storage lifecycle. Exposes a separate control API for model-tier operations. |
| `terminus-rs` | MCP tool hub: a single gateway exposing 100+ tools (version control, project tracking, monitoring, web research, calendar, and more) with rate limiting and per-tool timeouts. |

Local inference is served by a local model server running natively for direct GPU
access. A cloud provider can be configured as an opt-in fallback for tasks that need
frontier reasoning.

## Data flow

A user message travels through the system as follows:

1. **Ingest.** `lumina-core` receives the message from the configured chat channel.
2. **Context assembly.** The orchestrator builds the prompt: persona + behavioral
   guidelines, the recent conversation window (working memory), and any relevant facts
   retrieved from semantic memory.
3. **Routing.** The assembled request is sent to `chord-proxy`, which classifies it and
   selects a cost tier (see [Model management](#model-management)).
4. **Inference + tools.** The selected model generates a response. If it requests tools,
   `chord-proxy` runs an agentic loop, dispatching tool calls to `terminus-rs` and feeding
   results back to the model until it produces a final answer.
5. **Respond.** The final answer is returned to the user on the chat channel.
6. **Persist.** Salient information from the exchange is written back into memory so it is
   available to future turns.

## Memory architecture

Memory is organized into three tiers, each optimized for a different time horizon:

| Tier | What it holds | Backing store | Lifetime |
|------|---------------|---------------|----------|
| **Working memory** | The current conversation window — recent turns kept verbatim, optionally summarized when the token budget is exceeded. | In-process buffer | Current session |
| **Episodic memory** | A rolling history of past interactions and events the assistant has lived through. | Local database | Days to weeks |
| **Semantic memory** | Long-term facts, preferences, decisions, and learned patterns, embedded for retrieval-augmented generation. | Local vector store (embeddings) | Indefinite |

When the working-memory token budget is exceeded, older turns are summarized rather than
dropped, so context is compressed instead of lost. Semantic memory is queried on every turn
to surface relevant long-term facts; new facts are written back after exchanges that produce
durable information. All three tiers live on the operator's own hardware.

## Model management

Inference is treated as a resource to be allocated, not a constant. Two mechanisms manage it.

### Cost-tier routing

Requests are routed to the cheapest tier that can satisfy them:

| Tier | Handles | Relative cost |
|------|---------|---------------|
| **Deterministic code** | Anything that does not require a language model — API calls, math, formatting, parsing, scheduling. | Free |
| **Local models** | Tasks needing language understanding or generation: summarization, classification, drafting. | Free (runs on your hardware) |
| **Cloud models** | Tasks needing frontier reasoning: complex synthesis, hard problem solving. | Per-token (opt-in) |

The proxy classifies each request and stops at the first tier that can do the job. See
[inference-debloating.md](inference-debloating.md) for the full decision chain.

### Storage-tier lifecycle

Independently of routing, *model files* are managed across three storage tiers so that the
right models are available quickly without exhausting disk or memory:

| Tier | Location | State |
|------|----------|-------|
| **Hot** | Resident in GPU/unified memory | Loaded, instant response |
| **Warm** | On local disk | Available to load on demand |
| **Cold** | Archived (e.g. network storage) | Pulled back to warm when needed |

A background sweep demotes idle warm models and archives under disk pressure; protected core
models are never auto-archived. A control API (separate listener, bearer-auth) lists models,
reports storage usage, and lets an operator pull, archive, protect, or sweep models on
demand. See [model-tier-control-api.md](model-tier-control-api.md).

## Design principles

- **Privacy-first.** The assistant's value should not require surrendering your data. Memory,
  conversations, and configuration stay on hardware you control; cloud inference is opt-in
  and used only when local models are not enough.
- **Local inference by default.** Open-weight models served locally are the default path.
  The marginal cost of routine operation should be effectively zero.
- **Encrypt at deployment.** Secrets are resolved from an encrypted vault at runtime, never
  hardcoded. Configuration is injected via environment variables. A PII/secret gate guards
  the repository.
- **Least privilege.** Tools reach external systems only through the single MCP hub, outbound
  network access is allowlisted, and sensitive operations are gated behind explicit operator
  approval.
- **Allocate inference, don't assume it.** Most tasks do not need a language model. The
  routing chain spends an LLM call only when judgment is genuinely required.

See the [module registry](modules.md) for what each module does, and
[deployment.md](deployment.md) for how to run it.
