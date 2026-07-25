// CGUI-09 (TERM #532): pure, side-effect-free helpers for the Models module. Kept apart
// from the React panels so the derivations (serving state, cost tier, radar transform,
// client-side text match) are unit-testable without mounting a component or a chart — the
// same pattern the audit notes for fleetRingBuffer/toolCatalog/forestEngine.
//
// Every function here is deterministic: given the same ModelListEntry / ModelDetailResponse
// it returns byte-identical output (no Date.now(), no RNG), so the mock-adapter fixtures make
// snapshot-style assertions stable.
import type { ModelListEntry, ModelDetailResponse } from '../../types/mint';
import type { PillState } from '../../components/StatusPill';
import type { BadgeTone } from '../../components/Badge';

// ── Serving state ────────────────────────────────────────────────────────────

export interface ServingDescriptor {
  state: PillState;
  label: string;
  /** whether the pill should pulse (only a live serve does). */
  pulse: boolean;
}

/** Map a roster entry's serving/fleet flags to a StatusPill descriptor. A live serve is
 *  `hot` (holding VRAM now); an in-fleet-but-cold model is `cold`; anything else `idle`. */
export function deriveServingState(entry: ModelListEntry): ServingDescriptor {
  if (entry.serving_now) return { state: 'hot', label: 'Serving', pulse: true };
  if (entry.in_current_fleet) return { state: 'cold', label: 'In Fleet', pulse: false };
  return { state: 'idle', label: 'Candidate', pulse: false };
}

// ── Cost / footprint tier ────────────────────────────────────────────────────

export interface CostTier {
  label: string;
  tone: BadgeTone;
}

/** Derive a self-hosted "cost" tier from a model's VRAM footprint (falling back to param
 *  count when VRAM is unknown). Local inference cost is dominated by the VRAM a model pins,
 *  so the tier doubles as a footprint indicator. Returns a neutral `—` when neither is known. */
export function deriveCostTier(entry: ModelListEntry): CostTier {
  const gb = entry.vram_gb ?? (entry.params_b != null ? entry.params_b * 0.6 : null);
  if (gb == null) return { label: '—', tone: 'neutral' };
  if (gb < 4) return { label: 'XS', tone: 'green' };
  if (gb < 12) return { label: 'S', tone: 'blue' };
  if (gb < 24) return { label: 'M', tone: 'violet' };
  if (gb < 48) return { label: 'L', tone: 'amber' };
  return { label: 'XL', tone: 'rose' };
}

/** The capability flags that are set on a roster entry, in a fixed order, for capability
 *  badges. Empty array when a model covers nothing (a fresh brochure candidate). */
export function coverageBadges(entry: ModelListEntry): Array<{ key: string; label: string }> {
  const c = entry.coverage;
  const out: Array<{ key: string; label: string }> = [];
  if (c.coder) out.push({ key: 'coder', label: 'Coder' });
  if (c.assistant) out.push({ key: 'assistant', label: 'Assistant' });
  if (c.agent) out.push({ key: 'agent', label: 'Agent' });
  if (c.serving) out.push({ key: 'serving', label: 'Serving-tested' });
  return out;
}

// ── Client-side free-text match ──────────────────────────────────────────────

/** A snappy client-side name/family/category contains-match, applied on top of whatever the
 *  server already filtered (the mock + http adapters both honor `q`, but re-matching locally
 *  keeps typing responsive without a round-trip per keystroke). Case-insensitive; an empty
 *  query matches everything. */
export function matchesQuery(entry: ModelListEntry, query: string): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return true;
  return (
    entry.model_name.toLowerCase().includes(q) ||
    (entry.family ?? '').toLowerCase().includes(q) ||
    (entry.category ?? '').toLowerCase().includes(q)
  );
}

// ── Per-model category radar transform ───────────────────────────────────────

export interface RadarAxis {
  /** the task category (radar spoke label). */
  category: string;
  /** the model's pass-rate on that category, 0–1, rounded to 3dp. */
  score: number;
}

export interface RadarModel {
  axes: RadarAxis[];
  /** false → render an empty/"no profile data yet" state instead of a degenerate chart. */
  hasData: boolean;
}

/** Fold a model-detail's catalog cells into radar spokes: one spoke per task category, the
 *  value being that category's best (max) non-null pass rate. Categories with only null
 *  pass rates are dropped. Deterministic order (sorted by category) so the chart and its
 *  table twin agree and snapshots are stable. `hasData` is false when nothing scored — the
 *  caller renders an empty state rather than a one- or zero-spoke chart. */
export function buildCategoryRadar(detail: ModelDetailResponse | null): RadarModel {
  const cells = detail?.catalog?.cells ?? [];
  const best = new Map<string, number>();
  for (const cell of cells) {
    if (cell.pass_rate == null) continue;
    const prev = best.get(cell.task_category);
    if (prev == null || cell.pass_rate > prev) best.set(cell.task_category, cell.pass_rate);
  }
  const axes: RadarAxis[] = Array.from(best.entries())
    .map(([category, v]) => ({ category, score: Math.round(v * 1000) / 1000 }))
    .sort((a, b) => a.category.localeCompare(b.category));
  return { axes, hasData: axes.length > 0 };
}

// ── Formatting ───────────────────────────────────────────────────────────────

/** Render a nullable ratio 0–1 as a percentage, or a mono em-dash when unknown. */
export function fmtPct(v: number | null | undefined): string {
  return v == null ? '—' : `${(v * 100).toFixed(0)}%`;
}

/** Render a nullable count/number, or a mono em-dash when unknown. */
export function fmtNum(v: number | null | undefined, suffix = ''): string {
  return v == null ? '—' : `${v}${suffix}`;
}

/** Render a nullable VRAM footprint in GB. */
export function fmtGb(v: number | null | undefined): string {
  return v == null ? '—' : `${v.toFixed(1)} GB`;
}
