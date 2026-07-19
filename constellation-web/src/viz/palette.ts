// CONST-17: the viz kit's palette module (§4.2). Brand-derived, validated hexes ONLY — no
// colors outside the brand system. The 6 categorical slots below are the SNAPPED values
// (validate_palette.js run against --mode dark --surface "#161130"; see README.md for the
// full report). Series ceiling is 6 (fold to "Other" beyond); all-pairs forms (scatter,
// radar, swarm) cap at 4.

export const CATEGORICAL: readonly string[] = [
  'var(--series-1)', // violet-400
  'var(--series-2)', // flux-green family (snapped)
  'var(--series-3)', // flux-amber family (snapped)
  'var(--series-4)', // flux-blue family (snapped)
  'var(--series-5)', // flux-rose
  'var(--series-6)', // violet-200 family (snapped)
];

/** Raw (non-CSS-var) hexes — for nivo/recharts props that can't resolve CSS custom
 *  properties at the SVG-attribute level (some nivo color scales want literal strings).
 *  MUST stay byte-identical to the --series-N values in globals.css. */
export const CATEGORICAL_HEX: readonly string[] = [
  '#A855F7',
  '#059669',
  '#D97706',
  '#1D4ED8',
  '#F43F5E',
  '#9D6FE0',
];

export const SEQUENTIAL_HEX: readonly string[] = [
  '#5B21B6', '#6D28D9', '#7C3AED', '#A855F7', '#C4A5FB', '#DDC9FD',
];

export const DIVERGING = {
  cold: '#3B82F6',
  mid: '#4B5563',
  hot: '#F43F5E',
} as const;

export const CHART_CHROME = {
  grid: '#221A40',
  axis: '#2C2350',
  deemphasis: '#4B5563',
} as const;

export const SERIES_CEILING = 6;
export const ALL_PAIRS_CEILING = 4;

/** §2.4 applied to charts: a series that IS a brand semantic wears its semantic token,
 *  never a categorical slot. Nominal identities (models, languages, providers) use the
 *  categorical slots. Never both meanings in one chart. */
export type SemanticSeries =
  | 'tier-hot' | 'tier-warm' | 'tier-cold'
  | 'cost-free' | 'cost-paid'
  | 'flow-source' | 'flow-core' | 'flow-endpoint' | 'flow-cloud'
  | 'health-success' | 'health-warning' | 'health-error' | 'health-info';

export const SEMANTIC_SERIES_HEX: Record<SemanticSeries, string> = {
  'tier-hot': '#F43F5E',
  'tier-warm': '#F59E0B',
  'tier-cold': '#3B82F6',
  'cost-free': '#10B981',
  'cost-paid': '#F59E0B',
  'flow-source': '#3B82F6',
  'flow-core': '#7C3AED',
  'flow-endpoint': '#10B981',
  'flow-cloud': '#F59E0B',
  'health-success': '#10B981',
  'health-warning': '#F59E0B',
  'health-error': '#F43F5E',
  'health-info': '#3B82F6',
};

/** Sequential ramp accessor: `t` in [0,1], 0 = darkest/lowest magnitude, 1 = lightest/highest
 *  (high = light so magnitude pops on deep space, §4.2). */
export function sequentialColor(t: number): string {
  const clamped = Math.min(1, Math.max(0, t));
  const idx = Math.round(clamped * (SEQUENTIAL_HEX.length - 1));
  return SEQUENTIAL_HEX[idx];
}

/** Diverging accessor: `t` in [-1,1], negative = cold, positive = hot, 0 = neutral mid. */
export function divergingColor(t: number): string {
  if (t === 0) return DIVERGING.mid;
  return t < 0 ? DIVERGING.cold : DIVERGING.hot;
}

/**
 * Stable categorical slot assignment: entities get a slot in FIRST-SEEN order and KEEP it
 * across filtering — color follows the entity, never rank (§4.2). Slot assignment is
 * per-chart-instance state, not per-render: instantiate one `SlotAssigner` per chart
 * component instance (e.g. via `useMemo(() => new SlotAssigner(), [])` or a ref) and reuse
 * it across re-renders/filter changes.
 */
export class SlotAssigner {
  private order: string[] = [];

  /** Returns the stable categorical color for `id`, assigning the next free slot on first
   *  sight. Beyond SERIES_CEILING, returns the deemphasis chrome color ("Other" fold). */
  colorFor(id: string): string {
    let idx = this.order.indexOf(id);
    if (idx === -1) {
      idx = this.order.length;
      this.order.push(id);
    }
    if (idx >= SERIES_CEILING) return CHART_CHROME.deemphasis;
    return CATEGORICAL_HEX[idx];
  }

  /** Returns the 0-based slot index for `id` without allocating (undefined if unseen). */
  slotOf(id: string): number | undefined {
    const idx = this.order.indexOf(id);
    return idx === -1 ? undefined : idx;
  }

  /** Whether `id` has exceeded the all-pairs (scatter/radar/swarm) cap of 4. */
  exceedsAllPairsCap(id: string): boolean {
    const idx = this.order.indexOf(id);
    return idx === -1 ? false : idx >= ALL_PAIRS_CEILING;
  }

  reset(): void {
    this.order = [];
  }
}
