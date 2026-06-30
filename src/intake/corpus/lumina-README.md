<p align="center">
  <img src="./assets/banner.svg" alt="Lumina Constellation" width="100%"/>
</p>

<h1 align="center">Lumina Constellation</h1>

<p align="center">
  <strong>A self-hosted, privacy-first personal AI assistant — written in Rust, local-inference-first.</strong>
</p>

<p align="center">
  <img src="./assets/badges.svg" alt="build · tests · rust 2024 edition · MIT · v1.0"/>
</p>

<p align="center">
  <a href="#what-is-lumina">What is it?</a> ·
  <a href="#key-features">Features</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#modules">Modules</a> ·
  <a href="docs/architecture.md">Docs</a> ·
  <a href="#license">License</a>
</p>

---

## What is Lumina

Lumina is a personal AI assistant you run on your own hardware. It is not a website you
visit or a cloud account you rent — it is a long-running service that lives on a machine
you control, holds a persistent memory of your context, and talks to you through a chat
channel of your choosing.

The design goal is simple: **the assistant should be useful without sending your life to a
third party.** By default, Lumina runs its inference locally against open-weight models
(via a local model server) and only reaches out to a cloud model when a task
genuinely needs frontier-level reasoning. For most day-to-day work — summarizing a page,
drafting a briefing, classifying a request — nothing ever leaves the box, and the marginal
cost of a "thought" is zero.

Lumina is built as a Rust workspace. The orchestration core, the inference proxy, and the
tool hub are all native binaries with no heavyweight runtime dependencies, so the whole
system fits comfortably on a single mini-PC, a workstation with a discrete GPU, or a
small cluster if you want to spread the load.

---

## Key features

- **Persistent, multi-turn memory.** A three-tier memory subsystem (working context, episodic
  history, semantic long-term store) means the assistant remembers what you told it last
  week, not just last message.
- **Personality system.** The assistant has a configurable persona, voice, and behavioral
  guidelines assembled into its system prompt at runtime — not a generic chatbot tone.
- **Tool calling via a unified MCP hub.** A single Model Context Protocol gateway exposes
  100+ tools (version control, project tracking, monitoring, web research, calendar, and
  more) to every agent, with per-call rate limiting and timeouts.
- **Tiered model management.** Models are tracked across **hot** (resident in GPU memory),
  **warm** (on local disk), and **cold** (archived) tiers, with automatic eviction under
  disk pressure and a control API for promotion/demotion.
- **Cost-aware routing.** Requests are classified and routed to the cheapest tier that can
  do the job: deterministic code first, local models next, cloud models only as a last
  resort. Spend is tracked per consumer.
- **Privacy-first by construction.** Secrets live in an encrypted vault, configuration is
  injected via environment variables, outbound network access is allowlisted, and a
  PII/secret gate guards every commit.

---

## Architecture

<p align="center">
  <img src="./assets/architecture.svg" alt="Lumina architecture" width="100%"/>
</p>

Lumina is composed of three Rust crates plus local model serving:

- **`lumina-core`** — the orchestrator. Owns the chat channel(s), assembles the system
  prompt and persona, runs the scheduler, and manages the three-tier memory subsystem.
- **`chord-proxy`** — the smart inference proxy. Classifies and routes requests across cost
  tiers, runs the agentic tool-calling loop, and manages the model storage lifecycle.
- **`terminus-rs`** — the MCP tool hub. A single gateway through which every agent reaches
  external systems, with rate limiting and per-tool timeouts.

A typical request flows: **user message → `lumina-core` assembles context + memory →
`chord-proxy` routes to the right model tier → the model may call tools via `terminus-rs` →
the response is returned and written back to memory.**

See [docs/architecture.md](docs/architecture.md) for the full picture, including the
memory and model-tier designs.

---

## Quick start

### Prerequisites

- A recent stable **Rust** toolchain ([rustup](https://rustup.rs) recommended)
- A **local model server** (model serving runtime) for local inference
- A **GPU with Vulkan or ROCm support** (or Apple Silicon with Metal) for fast local
  inference — CPU-only works but is slow
- A **chat channel** the assistant can talk on (e.g. a Matrix homeserver, self-hosted or
  public)

### Build

```bash
git clone https://github.com/<your-org>/lumina-constellation.git
cd lumina-constellation
cargo build --workspace --release
```

### Configure

All configuration is supplied through environment variables. Copy the example file and
fill in your own values:

```bash
cp .env.example .env
$EDITOR .env
```

At minimum you will set your chat-channel credentials, your local model URL, and the
inference proxy's signing secret. Every variable is documented in
[docs/deployment.md](docs/deployment.md#configuration-reference). **Never commit a
populated `.env`** — secrets belong in the vault.

### Run

```bash
# Pull a local model into your model server (example: qwen3:8b)

# Start the proxy and orchestrator (see docs/deployment.md for service files)
./target/release/chord-proxy &
./target/release/lumina-core
```

Then say hello on your configured chat channel. For single-host and multi-host setups,
service definitions, and secrets management, see [docs/deployment.md](docs/deployment.md).

---

## Modules

Lumina ships a roster of cooperating modules — memory, briefings, monitoring, research,
notifications, cost governance, and more. Each does one thing well and shares the same
memory and tool hub.

See the full registry in **[docs/modules.md](docs/modules.md)**.

---

## Model profiling harnesses

Lumina decides which local model plays which role from *measured evidence*, not vibes. Two
harnesses profile every candidate model and write their results to a shared schema, keyed on
the **same model IDs and hardware backend tags** so the two profiles join in one
`model_dual_profile` view. Both run all inference through the unified inference proxy — they
are clients of the proxy, never of a backend socket.

- **[Coder / builder intake (MINT v2)](docs/harnesses/coder-intake.md)** — the
  `model_intake` tool. A graduated nine-tier context-stress probe plus a 40-case
  realistic-build code suite (Rust / Python / TypeScript / Bash), graduated 0–5 scoring with
  a two-phase batched idiom judge, profiled per hardware backend (GPU / CPU). Answers *"can
  this model build software, and how big a context can it really hold?"*

- **[Assistant intake](docs/harnesses/assistant-intake.md)** — six assistant dimensions
  (conversation depth, tool chaining, memory integration, latent personality, prompted
  adherence, embeddings), a mix of deterministic metrics and a 3-judge OAuth-CLI panel
  (mean + SD), a dim-4-shortlist-then-dim-5-rank personality read that is never merged, and a
  chat-role routing guard. Answers *"is this model a good assistant, and should it be the
  chat alias?"*

---

## Documentation

- [Architecture](docs/architecture.md) — system design, data flow, memory, model tiers
- [Deployment](docs/deployment.md) — prerequisites, configuration, single/multi-host, secrets
- [Modules](docs/modules.md) — the module registry
- [Coder intake harness](docs/harnesses/coder-intake.md) — builder-side model profiling (`model_intake`, MINT v2)
- [Assistant intake harness](docs/harnesses/assistant-intake.md) — assistant-side model profiling (six dimensions)
- [Inference de-bloating](docs/inference-debloating.md) — the routing philosophy that keeps cost near zero
- [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md)

---

## License

Lumina Constellation is released under the [MIT License](LICENSE).
