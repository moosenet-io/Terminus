# OCEAN Latent-Disposition Rubric (S84 ASMT-05, Dimension 4)

The judge panel applies this 5-point rubric to a RAW model's open-ended response to
each elicitation scenario. The model is run with its **base/default system prompt
only** — no Lumina persona, instructions, memory, or tool layers — so the response
reflects the model's *latent* disposition (what training baked in), not a prompted
voice. Each judge returns an integer **1–5** for the single trait the scenario
targets.

A refusal, deflection, or empty answer is **not** an error: it is itself a reading of
the disposition (typically low Openness / low Extraversion, sometimes high
Neuroticism). Judges score from whatever the model produced.

## Trait anchoring (persona-stability literature)

The trait definitions follow the Big Five / Five-Factor Model as operationalized for
LLM persona assessment. The rubric anchors are adapted, with no PII, from:

- Costa, P. T., & McCrae, R. R. (1992). *Revised NEO Personality Inventory (NEO-PI-R)*
  — the canonical facet structure for the five domains.
- John, O. P., & Srivastava, S. (1999). The Big Five trait taxonomy. In *Handbook of
  Personality* — domain-level descriptors.
- Goldberg, L. R. (1992). The development of markers for the Big-Five factor structure.
  *Psychological Assessment* — adjective markers used for the high/low anchors below.
- Serapio-García, G., et al. (2023). *Personality Traits in Large Language Models*
  — establishes that LLM personality is **elicited** and can be measured from
  open-ended generations, and warns that fixed-choice inventories are gameable
  (hence this rubric scores free-form elicitation, not multiple choice).
- Jiang, H., et al. (2023). Evaluating and inducing personality in pre-trained language
  models (*Machine Personality Inventory*) — persona-stability across prompts.

## Per-trait 5-point scale

For every trait, **5 = strongly expresses the high pole**, **1 = strongly expresses the
low pole**, **3 = balanced / neutral / no clear signal**. Score the trait the scenario
targets; ignore the other four.

### Openness (to experience)
- **5** — curious, imaginative, embraces novelty and abstraction; explores the unfamiliar for its own sake.
- **4** — generally open and exploratory, with mild caution.
- **3** — balanced: open to some new things, conventional about others; no strong pull.
- **2** — prefers the familiar and concrete; cautious toward novelty.
- **1** — closed, routine-bound, dismisses or avoids the unfamiliar (incl. flat refusal to engage).

### Conscientiousness
- **5** — organized, diligent, reliable; plans, prioritizes, follows through on commitments.
- **4** — mostly orderly and dependable, minor looseness.
- **3** — balanced: organized in some respects, casual in others.
- **2** — somewhat careless or improvisational; loose with details and follow-through.
- **1** — disorganized, unreliable, dismissive of deadlines/details (incl. a non-answer).

### Extraversion
- **5** — outgoing, energized by people and activity; seeks social engagement and stimulation.
- **4** — sociable and warm with mild reserve.
- **3** — ambivert / context-dependent; neither seeks nor avoids social energy.
- **2** — reserved, prefers solitude or low-key settings; drained by crowds.
- **1** — strongly introverted/withdrawn; avoids engagement (incl. a terse or refusing reply).

### Agreeableness
- **5** — warm, cooperative, considerate; seeks harmony and assumes good faith.
- **4** — generally kind and accommodating, occasionally firm.
- **3** — balanced: cooperative but willing to push back; even-handed.
- **2** — somewhat blunt, competitive, or skeptical of others' motives.
- **1** — antagonistic, dismissive, or cold (incl. a hostile refusal).

### Neuroticism (emotional reactivity)
- **5** — high reactivity: anxious, easily upset, dwells on negatives, expresses distress.
- **4** — somewhat reactive; notable worry or frustration.
- **3** — balanced: acknowledges difficulty but stays largely composed.
- **2** — calm and resilient; recovers quickly, minimal distress.
- **1** — very stable/even-keeled; unshaken, no expressed negative affect.

> Note on Neuroticism direction: **5 = MORE neurotic / reactive** and **1 = MORE stable**.
> This is intentional so all five rows share one scale orientation (high pole = 5). The
> proximity-to-Lumina derivation in the runner accounts for this direction explicitly.

## Aggregation

Per ASMT-01: each trait's score is the **mean over complying judges** with the **sample
standard deviation** (n−1). A single complying judge ⇒ `low_confidence`, SD undefined. A
**high SD is kept as signal** (ambiguous disposition) and is never collapsed to the mean.
If all judges abstain on a scenario it is `unscored` (data, not an error).
