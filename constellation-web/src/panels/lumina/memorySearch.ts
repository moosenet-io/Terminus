// LGUI-08 (§3.3): pure, dependency-free helpers for the Memory browser panel — kept separate
// from `useLuminaMemory.ts` so they're trivially unit-testable (vitest, see
// `memorySearch.test.ts`) without touching React/hook machinery.
import type { Memory, MemorySearchParams, MemoryType, SensitivityCategory } from '../../types/luminaMemory';

/** Builds the exact `GET /api/engram/search?...` query string (§7) from filter state. Omits
 *  empty/undefined fields entirely (never sends `q=` or `type=` blank) so the server (and the
 *  mock adapter, which must apply these params server-side per §3.3) sees a clean param set. */
export function buildMemorySearchQuery(params: MemorySearchParams): string {
  const usp = new URLSearchParams();
  if (params.q && params.q.trim()) usp.set('q', params.q.trim());
  if (params.type) usp.set('type', params.type);
  if (params.sensitivity) usp.set('sensitivity', params.sensitivity);
  if (params.visibility) usp.set('visibility', params.visibility);
  if (params.user) usp.set('user', params.user);
  if (params.limit != null) usp.set('limit', String(params.limit));
  const qs = usp.toString();
  return qs ? `/engram/search?${qs}` : '/engram/search';
}

/** Applies `MemorySearchParams` to an in-memory fixture array — this is the ONE place that
 *  simulates server-side filtering for the mock adapter (§3.3: "the mock adapter must apply the
 *  query params; never client-mine a full dump" — i.e. no panel/hook code may re-filter a full
 *  dump client-side; this function stands in for the real backend's query logic in mock mode
 *  only). A real `httpAdapter` call never runs this — the actual server filters. */
export function applyMemorySearchParams(all: Memory[], params: MemorySearchParams): Memory[] {
  let rows = all;
  if (params.type) {
    const t = params.type;
    rows = rows.filter(m => m.memory_type === t);
  }
  if (params.sensitivity) {
    const s = params.sensitivity;
    rows = rows.filter(m => m.sensitivity === s);
  }
  if (params.visibility) {
    const v = params.visibility;
    rows = rows.filter(m => m.visibility === v);
  }
  if (params.user) {
    const u = params.user;
    rows = rows.filter(m => m.user_id === u);
  }
  if (params.q && params.q.trim()) {
    const needle = params.q.trim().toLowerCase();
    rows = rows.filter(m => m.content.toLowerCase().includes(needle));
  }
  const limit = params.limit ?? 50;
  return rows.slice(0, limit);
}

/** §3.3 results table: "content preview (2-line clamp)". Line-clamping itself is CSS
 *  (`WebkitLineClamp`, applied where this is rendered); this just guards pathologically long
 *  single-line content (spec's "huge content for clamp testing" fixture) with a hard character
 *  cap so a table row can never blow out layout even before CSS clamp engages, and so the
 *  Drawer can show a distinct "(showing full content below)" affordance is unnecessary — the
 *  Drawer always renders the untruncated `content` field, this helper is preview-only. */
const PREVIEW_MAX_CHARS = 240;

export function clampPreview(content: string): string {
  if (content.length <= PREVIEW_MAX_CHARS) return content;
  return `${content.slice(0, PREVIEW_MAX_CHARS - 1).trimEnd()}…`;
}

/** Fixed Badge tone mapping (§5 / §3.3) — the ONE place `MemoryType` maps to a tone, so
 *  `MemoryTypeBadge` and the header legend can never disagree. */
export const MEMORY_TYPE_TONE: Record<MemoryType, 'violet' | 'blue' | 'green' | 'neutral'> = {
  Principle: 'violet',
  Semantic: 'blue',
  Preference: 'green',
  Episodic: 'neutral',
};

/** Walks a `superseded_by` chain starting at `id` to detect a cycle (defensive — a well-formed
 *  store should never produce one, but the Drawer's "navigate the chain" affordance must not
 *  infinite-loop on malformed/mock data). Returns the ids visited, in order, stopping at the
 *  first repeat or a dead end. */
export function supersededChain(byId: Map<string, Memory>, startId: string): string[] {
  const seen = new Set<string>();
  const chain: string[] = [];
  let current: string | null = startId;
  while (current && !seen.has(current)) {
    seen.add(current);
    chain.push(current);
    const rec: Memory | undefined = byId.get(current);
    current = rec?.superseded_by ?? null;
  }
  return chain;
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = bytes / 1024;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex++;
  }
  return `${value.toFixed(value < 10 ? 2 : 1)} ${units[unitIndex]}`;
}

export function formatSensitivity(s: SensitivityCategory): string {
  return s;
}
