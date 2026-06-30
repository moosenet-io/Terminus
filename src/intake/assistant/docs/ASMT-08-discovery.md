# ASMT-08 — New-Model Discovery & Nomination (Lumina chat fleet)

- **Spec:** S84-assistant-intake-profiling
- **Item:** ASMT-08 (discovery / documentation)
- **Audience:** <operator> (operator) and the ASMT-09 acquisition agent
- **Refreshed:** 2026-06-24 (the seed list was researched earlier in S84; this run revalidated it against
  current Ollama/HF catalogs and preference leaderboards — the landscape moves monthly)
- **Output:** `../nominations.json` (one record per model, S83-consistent IDs)

> Infrastructure is referred to by **ROLE** only (primary inference host, large-model host, summarizer).
> No IPs, hostnames, container numbers, or paths. The pii_gate (internal posture) and the task's PII-free
> requirement both apply.

---

## Method (~250 words)

Candidates are filtered on **preference / persona signals**, not coding ability. The discovery question
is "which model can *be Lumina*" — hold a defined voice across turns, survive the 3-tier memory pipeline,
and obey behavioral rules under pressure — which is a different axis from S83/MINT's builder profiling
(HumanEval/SWE-bench), and a top coder can be a poor conversationalist. Sources, in priority order:

1. **Chatbot Arena / LMArena** (`arena.ai/leaderboard/text`) — crowd-voted Elo on open-ended dialogue;
   the closest proxy for "people prefer talking to this model." Used for relative ranking within a size
   class, not absolute.
2. **MT-Bench / AlpacaEval / Arena-Hard** — judge-scored instruction-following and multi-turn coherence.
   (Mistral Small 3.2's instruct refinement nearly doubling Arena-Hard, 19.56%→43.10%, is the kind of
   signal that matters here.)
3. **Persona-consistency benchmarks — RPBench-Auto (Boson AI) and Fiction.liveBench** — measure holding a
   character / tracking entities across long context, the dim-5 (prompted adherence) analogue.

For each candidate we record: **size class + active params** (must fit the primary inference host's
~96GB VRAM budget), **license** (prefer Apache-2.0 / MIT; non-commercial flagged), **source leaderboard
+ score note**, and a **gfx1151 runnability class**. NEW = not in the S83 builder fleet
(`qwen3-coder:30b`, `qwen3:8b`, `gpt-oss:20b`, `diffusiongemma-26b-a4b`). Model IDs use the S83 Ollama-
style `name:tag` form so the `model_dual_profile` join in ASMT-01 lines up byte-for-byte. Coders are not
disqualified — the seed deliberately spans lineages (Cohere, IBM, Google, Mistral, Qwen, Swiss-AI, AI2,
TII, Microsoft, HuggingFace, MiniMax) to widen the **latent OCEAN spread** the dim-4 read depends on:
a model whose training corpus differs from the Qwen/Llama mainstream gives a genuinely different base
disposition, which is signal, not noise.

---

## gfx1151 runnability classification (~200 words)

Carried forward from existing fleet notes. The primary inference host is a Strix-Halo-class iGPU
(gfx1151) with **Vulkan (RADV) as the primary path**. Three classes:

- **confirmed** — dense transformer, validated family (or close sibling) already runs on the Vulkan path.
  Trust without a smoke test. (Command R, Granite 4.1, Gemma 4, Mistral Small 3.2, Phi-4 family, SmolLM3.)
- **experimental** — **MoE families hang on Vulkan** per fleet notes; route them via **ROCm with
  `HSA_OVERRIDE_GFX_VERSION`** set to a supported target so the ROCm runtime engages on gfx1151. Also
  here: **hybrid-SSM (Mamba2)** architectures (Falcon-H1) whose kernels are unproven on this path, and
  any new-to-fleet dense lineage that warrants a smoke test (Apertus 70B, Olmo 3.1 32B). MiniMax M3
  (MoE + sparse attention) is the canonical experimental case.
- **unknown** — no datapoint and non-trivial architecture; ASMT-09 runs a bounded 1-case smoke before the
  full suite.

**Large-model knob:** set `OLLAMA_FLASH_ATTENTION=1` for the 70B-class (Apertus) so attention memory
stays within budget. Acquisition strategy follows the class: Vulkan-first for confirmed; ROCm+HSA-override
for experimental MoE; mark a model **skipped-with-reason** if it hangs on both paths (do not crash the run).

---

## Annotated nomination list

14 records (12 seed concepts; Granite and Phi each split into two size points; Command A+ retained as a
documented over-ceiling skip). Full machine-readable records in `../nominations.json`.

### Tier A — confirmed-class, comfortable fit

1. **`command-r:latest`** — Cohere, **35B dense** (seed said ~32B; actual 35B), CC-BY-NC. Conversation- and
   grounding/citation-first — the best fit for an assistant that cites its Engram memory. *confirmed.*
   *(License is non-commercial; fine for a personal assistant, flag if scope changes.)*
2. **`granite4.1:8b`** — IBM, **8B dense**, Apache-2.0. RL post-trained for conversation + tool-calling,
   signed weights, predictable latency. *confirmed.* (Seed wrote "Granite 4.1"; family is 3B/8B/30B.)
3. **`granite4.1:30b`** — IBM, **30B dense**, Apache-2.0. Full-depth Granite for the quality alias. *confirmed.*
4. **`gemma4:12b`** — Google, **12B dense**, Gemma ToU. New SIZE point (S83 has no 12B). Native system
   role + function-calling; released 2026-06-03, on Ollama now. *confirmed.*
5. **`mistral-small3.2:24b`** — **24B dense**, Apache-2.0. **Seed correction:** "Mistral Small 4" does not
   exist; latest is **3.2**. Best agentic/function-calling at this size for dim-2 tool chaining. *confirmed.*
6. **`qwen3.5:27b`** — Alibaba, **27B dense**, Apache-2.0. **Seed correction:** "Qwen3.6 dense chat" does
   not exist — Qwen3.6 is a CODER family and its open 35B-A3B is **MoE**. The dense chat point is
   Qwen3.5 27B (2026-02-24). Pick the **dense** variant to stay confirmed-class; the Qwen3.6 MoE would be
   experimental (Vulkan hang → ROCm+HSA). Fallback `qwen3:32b` is on Ollama today. *confirmed.*

### Tier B — fits, distinctive disposition worth measuring

7. **`apertus:70b`** — Swiss-AI (EPFL/ETH), **70B dense**, Apache-2.0, fully open (weights+data+recipe).
   Distinct latent OCEAN profile. HF-only; smoke-test; `OLLAMA_FLASH_ATTENTION` for the 70B class.
   *experimental.*
8. **`olmo3.1:32b`** — AI2, **32B dense**, Apache-2.0. **Seed correction:** latest is **3.1** (Instruct
   32B). Fully-open lineage, transparency baseline. Community GGUF only; smoke-test. *experimental.*
9. **`falcon-h1:34b`** — TII, **34B hybrid attention+Mamba2**, Falcon license. **Seed correction:** the
   latest 34B is the **hybrid** Falcon-H1, which raises the runnability risk (SSM kernels on Vulkan).
   HF GGUF / llama.cpp-native; smoke-test, ROCm fallback. *experimental.*
10. **`minimax-m3:latest`** — **MoE + sparse attention**, open-weights. **Seed correction:** M3
    (2026-06-01) supersedes M2; total params undisclosed → **footprint UNVERIFIED**, may exceed ceiling.
    **Also: NOT found in the S83 repo** — the seed's "already acquired in S83 addendum" did not check out,
    so treat as a fresh acquisition. Reputation for long-term persona retention makes it the
    highest-value MoE cross-test *if* it fits. Weights rolling out; verify upload + size first.
    *experimental.*

### Tier C — small + fast, low-latency chat alias

11. **`phi4:14b`** — Microsoft, **14.7B dense**, MIT. Strong instruction-following per param. *confirmed.*
12. **`phi4-mini:3.8b`** — Microsoft, **3.8B dense**, MIT, ~3GB. Snappy alias floor. *confirmed.*
13. **`smollm3:3b`** — HuggingFace, **3B dense**, Apache-2.0, fully open, dual-mode reasoning. Beats
    Llama3.2-3B / Qwen2.5-3B at the tier. Official GGUF; community Ollama only. *confirmed.*
14. **`command-a-plus:latest` — SKIP (documented upper bound).** Cohere **218B sparse MoE / 25B active**,
    Apache-2.0, W4A4. Cohere's own floor is ~2×H100 / 1×Blackwell — **exceeds the ~96GB ceiling.** Expect a
    clean skip-with-reason in ASMT-09. (Sibling Command A 111B dense is borderline-to-over AND CC-BY-NC, so
    not a substitute.) Listed to document the ceiling and to revisit if the inference host changes.

**Flagged, NOT acquired (single low-authority source):** community persona fine-tunes (Rocinante-12B-,
Snowpiercer-15B-class). Measure, don't trust — excluded from the acquisition list.

---

## Research anchors (cite, no PII)

- **LLM-as-jury** — juries reduce single-judge bias; report variance as signal (informs the dim-4/dim-5
  panel mean+SD design, ASMT-01).
- **Persona-stability literature** — persona-vector trait control, the "assistant-axis" default-persona
  work, and persona-consistency benchmarks (RPBench-Auto / PingPong / Fiction.liveBench) — inform the
  preference filter here and the dim-5 prompted-adherence rubric.
- **Fleet facts** — Vulkan (RADV) primary on gfx1151; MoE families hang on Vulkan → ROCm +
  `HSA_OVERRIDE_GFX_VERSION`; `OLLAMA_FLASH_ATTENTION=1` for the 70B class.
