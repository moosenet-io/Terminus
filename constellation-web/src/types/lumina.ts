// LGUI-09: shapes for the Lumina Persona & Behavior panel, bound exactly to
// LUMINA-GUI-SPEC.md §7 ("Data contracts — the new Lumina JSON API") and §0.1.1's
// `TraitVector {flair, spontaneity, humor, focus}` / soft-bound / `PromptAssembler` layer
// order. These are §7's response sketches for the persona routes — the mock adapter below
// (aggregationClient.ts) returns objects satisfying these shapes so the panel builds/runs
// identically against mock and http modes; the http adapter's generic `request<T>()`
// passthrough needs no per-endpoint code, just these types at the call site.
//
// Sibling build items (LGUI-06 overview, LGUI-07 chat, …) land their own `Lumina*` types in
// this same file for their own §7 routes — this item only adds the persona-shaped ones below,
// deliberately not touching/duplicating a `LuminaStatus` etc. if a sibling branch already
// defines one; see `useLuminaPersona.ts`'s own narrower status-flags fetch for why this item
// only needs two fields of `GET /api/status`, not the whole contract.

/** `TraitVector` (`crates/lumina-core/src/prompt/traits.rs`) — the four assistant behavior
 *  dials, each a float in the soft-bound range (`PERSONA_BOUNDS` below). */
export interface LuminaTraitVector {
  flair: number;
  spontaneity: number;
  humor: number;
  focus: number;
}

export type LuminaTraitKey = keyof LuminaTraitVector;

export const LUMINA_TRAIT_KEYS: readonly LuminaTraitKey[] = ['flair', 'spontaneity', 'humor', 'focus'];

/** §0.1.1: soft bounds shared by the client-side clamp (mirrors the server's own clamp on
 *  `effective = clamp(base + modifier)`, `prompt/multi_personality.rs`). */
export interface LuminaPersonaBounds {
  min: number;
  max: number;
}

export const PERSONA_DEFAULT_BOUNDS: LuminaPersonaBounds = { min: 0.15, max: 0.85 };

/** One `PromptAssembler` layer (`prompt/mod.rs`) — the 11 fixed-order layers `[identity]
 *  [rules][capabilities][style][personality][opinions][knowledge][context][memory]
 *  [proactive][now]` (§0.1.1). Read-only in the Layer Inspector. */
export interface LuminaPromptLayer {
  name: string;
  bytes: number;
  enabled: boolean;
}

/** The 11 assembler layer names, in `PromptAssembler`'s fixed order (§0.1.1) — used to
 *  validate/order the mock + real API response so the Layer Inspector never silently
 *  reorders or drops a layer. */
export const LUMINA_PROMPT_LAYER_ORDER: readonly string[] = [
  'identity', 'rules', 'capabilities', 'style', 'personality', 'opinions', 'knowledge',
  'context', 'memory', 'proactive', 'now',
];

/** `GET /api/persona?user=` (§7). */
export interface LuminaPersonaResponse {
  traits: {
    base: LuminaTraitVector;
    modifier: LuminaTraitVector;
    effective: LuminaTraitVector;
  };
  bounds: LuminaPersonaBounds;
  knowledge_digest: string;
  active_context: string;
  layers: LuminaPromptLayer[];
}

/** `PUT /api/persona/traits` (admin) request body (§7) — either side may be omitted (e.g. an
 *  admin-on-behalf modifier-only edit leaves `base` untouched). `user` selects whose modifier
 *  is being written; absent = the acting admin's own. */
export interface LuminaPersonaTraitsWriteBody {
  base?: LuminaTraitVector;
  modifier?: LuminaTraitVector;
  user?: string;
}

/** `PUT /api/persona/traits` response — server-computed, re-clamped effective (never trust a
 *  client-side clamp as the source of truth for what got saved). */
export interface LuminaPersonaTraitsWriteResponse {
  base: LuminaTraitVector;
  modifier: LuminaTraitVector;
  effective: LuminaTraitVector;
}

/** `PUT /api/persona/context` (admin) (§7). */
export interface LuminaPersonaContextWriteBody {
  active_context: string;
}
export interface LuminaPersonaContextWriteResponse {
  active_context: string;
}

/** The narrow slice of `GET /api/status` (§7) the Persona panel's Ceremony card + "legacy
 *  prompt mode" warning need — NOT the full status contract (that's LGUI-06 Overview's
 *  `LuminaStatus`, a sibling type in this same file once that item lands). Kept separate and
 *  narrow deliberately: this item only reads two fields, so it doesn't need to wait on or
 *  duplicate the Overview panel's fuller shape. */
export interface LuminaPersonaStatusFlags {
  onboarding_complete: boolean;
  dynamic_prompt: boolean;
}
