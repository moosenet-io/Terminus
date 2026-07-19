// LGUI-08 (§3.3 "lumina.memory — Engram browser"): shapes for the Memory (engram) browser
// panel, bound to LUMINA-GUI-SPEC.md §7's `GET /api/engram/stats` and
// `GET /api/engram/search?q=&type=&sensitivity=&visibility=&user=&limit=` contracts.
//
// Deliberately a SEPARATE file from `src/types/lumina.ts` (LGUI-06's Overview-panel types,
// unmerged sibling branch as of this build) rather than extending it — same filename-collision
// avoidance the sibling LGUI-06/07 branches call for; reconciling the two `LuminaEngramStats`
// shapes (this file's is a superset used by the panel's stats strip) happens at merge time, not
// here.

/** §3.3 filter row: the four `MemoryType`s, fixed Badge tone mapping (§5):
 *  violet=Principle, blue=Semantic, green=Preference, neutral=Episodic. */
export type MemoryType = 'Episodic' | 'Semantic' | 'Preference' | 'Principle';

/** §3.3: "7 categories" for the sensitivity filter. `Health`/`Finance`/`Personal` are the
 *  `is_always_private` set called out explicitly in the spec — those three ALWAYS carry the
 *  lock glyph on `SensitivityBadge` regardless of the record's actual `visibility`. */
export type SensitivityCategory =
  | 'None'
  | 'Personal'
  | 'Health'
  | 'Finance'
  | 'Work'
  | 'Relationships'
  | 'Location';

export const SENSITIVITY_CATEGORIES: readonly SensitivityCategory[] = [
  'None', 'Personal', 'Health', 'Finance', 'Work', 'Relationships', 'Location',
];

/** The always-private set (§3.3 / §0.1.3's `is_always_private`) — these categories carry the
 *  lock glyph unconditionally, independent of `Memory.visibility`. */
export const ALWAYS_PRIVATE_SENSITIVITIES: ReadonlySet<SensitivityCategory> = new Set([
  'Health', 'Finance', 'Personal',
]);

export function isAlwaysPrivate(sensitivity: SensitivityCategory): boolean {
  return ALWAYS_PRIVATE_SENSITIVITIES.has(sensitivity);
}

export type MemoryVisibility = 'Private' | 'Shared' | 'System';

/** §3.3 Drawer: "provenance (source conversation/turn)". */
export interface MemoryProvenance {
  conversation_id: string | null;
  turn_index: number | null;
  source: string;
}

/** A single engram record as returned by `GET /api/engram/search` — `embedding` is NEVER
 *  present in this shape (§7: "embedding OMITTED — never ship vectors"); there is deliberately
 *  no field for it here, not even an optional one, so a stray render of it is a type error, not
 *  a runtime accident. */
export interface Memory {
  id: string;
  memory_type: MemoryType;
  sensitivity: SensitivityCategory;
  visibility: MemoryVisibility;
  content: string;
  confidence: number;
  created_at: string;
  access_count: number;
  user_id: string | null;
  provenance: MemoryProvenance;
  /** Chain: this record's replacement, if any — the Drawer renders it as a navigable link
   *  (§3.3 "superseded_by link"). `null` = current / not superseded. */
  superseded_by: string | null;
}

/** `GET /api/engram/stats` (§7 + §3.3 stats strip). */
export interface LuminaMemoryStats {
  total: number;
  by_type: Record<MemoryType, number>;
  by_sensitivity: Record<SensitivityCategory, number>;
  db_bytes: number;
  embedded_pct: number;
  store_ok: boolean;
  /** Present only when `store_ok` is false — names the offending key ENV NAME only (S7 secrets
   *  discipline: never a value), e.g. `"ENGRAM_DB_KEY"`. Also covers a `SecurityViolation` on
   *  open (§3.3: "store health (key OK / SecurityViolation -> error card naming the key env"). */
   security_violation_key?: string;
}

/** Query params for `GET /api/engram/search` (§7) — all server-side (§3.3: "SERVER-SIDE
 *  filtering only — the mock adapter must apply the query params; never client-mine a full
 *  dump"). `user` is admin-only (§3.3 "user scope (admin only)"). */
export interface MemorySearchParams {
  q?: string;
  type?: MemoryType;
  sensitivity?: SensitivityCategory;
  visibility?: MemoryVisibility;
  user?: string;
  limit?: number;
}

export interface MemorySearchResponse {
  results: Memory[];
}
