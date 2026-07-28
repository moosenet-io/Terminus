// CONST-25 (§3.2, source 3 "entity search"): fans a palette query out to a handful of CHEAP,
// already-existing list endpoints (the exact same reads `aggregationClient.ts`'s mock adapter
// already serves — see MOCK_GET in that file) and ranks the combined hits with the palette's own
// fuzzy matcher. Every source is independent: one dead/erroring backend degrades ONLY its own
// group, it never breaks navigation or the other groups (§3.2 requirement) — `searchEntities`
// uses `Promise.allSettled`, never `Promise.all`, for exactly this reason.
//
// This is intentionally NOT exhaustive — it lists a handful of representative, low-cost
// "give me a name" endpoints per system (sessions, agent activity, providers, models, terminus
// modules). Deeper per-entity search (e.g. full-text over task bodies) is out of scope for
// CONST-25 and would need its own paginated/query-param'd endpoint, not a fan-out over list
// reads.
import type { AggregationClient } from './aggregationClient';
import type { Provider } from '../types/provider';
import type { ChordModelRecord } from '../types/chord';
import { rankItems } from './commandMatch';

export interface EntityHit {
  /** Stable id, unique within its group. */
  id: string;
  /** Palette row label. */
  label: string;
  /** Optional secondary text (status, model, etc.). */
  sublabel?: string;
  /** Group heading shown in the palette, e.g. "Sessions". */
  group: string;
  /** Where selecting this hit should navigate. Entity hits with no natural panel route are
   *  omitted from `EntitySourceResult` rather than given a dead link. */
  path: string;
}

export interface EntitySourceResult {
  group: string;
  status: 'ok' | 'error';
  hits: EntityHit[];
}

interface RawSession { id?: string; session_id?: string; name?: string; status?: string }
interface RawAgent { agent_id: string; display_name: string; provider: string; status: string }
interface RawTerminusModule { name: string; enabled: boolean; version?: string }

interface EntitySource {
  group: string;
  /** Fetches the raw list and maps it to hits. Throws (or rejects) on backend failure — the
   *  caller catches per-source, this function itself stays a pure best-effort mapper. */
  load: (client: AggregationClient) => Promise<EntityHit[]>;
}

const SOURCES: EntitySource[] = [
  {
    group: 'Sessions',
    async load(client) {
      const data = await client.request<{ sessions: RawSession[] }>('harmony', '/sessions');
      return (data.sessions ?? []).map((s, i) => {
        const id = s.session_id ?? s.id ?? String(i);
        return {
          id,
          label: s.name ?? `Session ${id}`,
          sublabel: s.status,
          group: 'Sessions',
          path: '/harmony/sessions',
        };
      });
    },
  },
  {
    group: 'Agents',
    async load(client) {
      const data = await client.request<{ agents: RawAgent[] }>('harmony', '/agents/activity');
      return (data.agents ?? []).map(a => ({
        id: a.agent_id,
        label: a.display_name,
        sublabel: `${a.provider} · ${a.status}`,
        group: 'Agents',
        path: '/harmony/agents',
      }));
    },
  },
  {
    group: 'Providers',
    async load(client) {
      const data = await client.request<Provider[]>('chord', '/providers');
      return (data ?? []).map(p => ({
        id: p.name,
        label: p.display_name || p.name,
        sublabel: p.status,
        group: 'Providers',
        path: '/chord/providers',
      }));
    },
  },
  {
    group: 'Models',
    async load(client) {
      const data = await client.request<ChordModelRecord[]>('chord', '/models');
      return (data ?? []).map(m => ({
        id: m.name,
        label: m.name,
        sublabel: m.loaded ? 'loaded' : m.storage_tier,
        group: 'Models',
        path: '/chord/inference',
      }));
    },
  },
  {
    group: 'Terminus modules',
    async load(client) {
      const data = await client.terminus.configSummary();
      return (data.modules ?? []).map((m: RawTerminusModule) => ({
        id: m.name,
        label: m.name,
        sublabel: m.enabled ? (m.version ?? 'enabled') : 'disabled',
        group: 'Terminus modules',
        path: '/terminus/config',
      }));
    },
  },
];

/**
 * Fans `query` out to every entity source in parallel, ranks each source's hits independently
 * with `rankItems`, and returns one `EntitySourceResult` per source — `status: 'error'` (empty
 * hits) for any source whose fetch rejected, so a dead backend never suppresses the others or
 * blocks navigation/action results (§3.2). Empty `query` returns no results from any source
 * (entity search only fires once the user has typed something — navigation/actions still show).
 */
export async function searchEntities(query: string, client: AggregationClient): Promise<EntitySourceResult[]> {
  if (query.trim().length === 0) return [];

  const settled = await Promise.allSettled(
    SOURCES.map(async source => {
      const raw = await source.load(client);
      const ranked = rankItems(query, raw, hit => `${hit.label} ${hit.sublabel ?? ''}`);
      return ranked.map(r => r.item);
    }),
  );

  return settled.map((result, i) => {
    const group = SOURCES[i].group;
    if (result.status === 'fulfilled') {
      return { group, status: 'ok' as const, hits: result.value };
    }
    return { group, status: 'error' as const, hits: [] };
  });
}
