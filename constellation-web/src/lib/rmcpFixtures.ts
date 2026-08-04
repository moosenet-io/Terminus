// ============================================================================================
// RMCP-13 (TERM-624) — THE ONE MOCKED BOUNDARY FOR THE CONNECTORS PAGE.
// ============================================================================================
//
// This file stands in for the SERVER, not for part of the client. It is reached only from
// `rmcpClient.ts`'s single `callTool` dispatch, and only when the app-wide adapter mode is
// `mock` (an explicit build flag, an explicit server injection, an explicit per-session opt-in,
// or a unit test — see `resolveMode()` in aggregationClient.ts). A production bundle resolves to
// `http` and never loads a value from here.
//
// Why it exists: the backing `rmcp_*` tools land in RMCP-05/06/07/08/11/12, in parallel with
// this item. Without a fixture server the page could not be built, exercised, or unit-tested at
// all. With one, the panels above are written against the real contract from the first line and
// swap to the live tools by doing nothing (the mode switch already exists).
//
// Two rules keep this honest:
//
//  1. **The matcher below is the MOCK SERVER's matcher, not a client-side resolver.** The panels
//     never call it; they call `previewGroup`/`resolveClientScope`, which in `http` mode go to
//     the real server. Nothing in `src/panels/` or `src/pages/` imports this file — that is the
//     line that must not be crossed, because the moment a panel resolves scope locally, the
//     preview stops being the server's answer and starts being a plausible guess.
//  2. **Nothing here is infrastructure.** Namespaces, tool names, and account names are generic
//     placeholders; there are no hosts, addresses, or real identities in this file.
import { RMCP_TOOLS, RmcpError } from './rmcpContract';
import type { RmcpToolName } from './rmcpContract';
import type {
  RmcpClient,
  RmcpResolvedScope,
  RmcpResolvedTool,
  RmcpServer,
  RmcpSession,
  RmcpToolGroup,
} from '../types/rmcp';

// ── Fixture catalog ─────────────────────────────────────────────────────────
// A merged, namespaced catalog shaped like the real one: a few namespaces, one of them down.

const FIXTURE_NAMESPACES: { namespace: string; available: boolean; tools: string[] }[] = [
  {
    namespace: 'media',
    available: true,
    tools: ['media_search', 'media_play', 'media_queue_add', 'media_library_scan', 'media_stats'],
  },
  {
    namespace: 'home',
    available: true,
    tools: ['home_light_set', 'home_scene_run', 'home_sensor_read', 'home_thermostat_set'],
  },
  {
    // Deliberately down, so the "scoped to an unavailable upstream" state is exercisable
    // offline. Its tools resolve as in-scope-but-unavailable, never as an error.
    namespace: 'workshop',
    available: false,
    tools: ['workshop_job_list', 'workshop_job_start'],
  },
  {
    namespace: 'notes',
    available: true,
    // Padded so the resolved preview's paging is exercisable without a live 400-tool catalog.
    tools: Array.from({ length: 60 }, (_, i) => `notes_entry_${String(i + 1).padStart(3, '0')}`),
  },
];

function qualified(namespace: string, tool: string): string {
  return `${namespace}::${tool}`;
}

/** MOCK-SERVER-SIDE matcher for the RMCP-06 pattern vocabulary (exact / trailing-`*` prefix /
 *  `<namespace>::*`). Rule 1 above: this is the fixture server's implementation of the rules,
 *  which is why it may exist at all — the browser-side code never matches patterns itself. */
function matchPattern(pattern: string, namespace: string, tool: string): boolean {
  const full = qualified(namespace, tool);
  if (pattern === '*') return true;
  const nsWildcard = pattern.match(/^([A-Za-z0-9_-]+)::\*$/);
  if (nsWildcard) return namespace === nsWildcard[1];
  if (pattern.endsWith('*')) {
    const prefix = pattern.slice(0, -1);
    // A bare `a*` matches on the QUALIFIED name, so it cannot leak across namespaces — the
    // spec's "`a*` must not match a tool in another namespace whose bare name starts with `a`".
    return full.startsWith(prefix);
  }
  return full === pattern || tool === pattern;
}

/** A pattern the mock server refuses at write time (RMCP-06: rejection is a write-time
 *  decision, never a match-time one). */
function patternRejection(pattern: string): string | null {
  const trimmed = pattern.trim();
  if (!trimmed) return 'empty pattern';
  if (trimmed !== pattern) return 'leading or trailing whitespace';
  if (/[^A-Za-z0-9_:*-]/.test(trimmed)) return 'unsupported characters — exact name, trailing * , or namespace::* only';
  if (trimmed.indexOf('*') !== -1 && trimmed.indexOf('*') !== trimmed.length - 1) {
    return 'a wildcard may only appear at the end';
  }
  return null;
}

function resolvePatterns(patterns: string[], groupName: string): RmcpResolvedTool[] {
  const out: RmcpResolvedTool[] = [];
  const seen = new Set<string>();
  for (const ns of FIXTURE_NAMESPACES) {
    for (const tool of ns.tools) {
      for (const pattern of patterns) {
        if (patternRejection(pattern)) continue;
        if (!matchPattern(pattern, ns.namespace, tool)) continue;
        const name = qualified(ns.namespace, tool);
        if (seen.has(name)) break;
        seen.add(name);
        out.push({
          name,
          namespace: ns.namespace,
          matchedGroup: groupName,
          matchedPattern: pattern,
          available: ns.available,
        });
        break;
      }
    }
  }
  return out.sort((a, b) => a.name.localeCompare(b.name));
}

// ── Mutable fixture state (per page load) ───────────────────────────────────

let groups: RmcpToolGroup[] = [
  { id: 'g-media', name: 'media', description: 'Library search and playback', patterns: ['media::*'], editable: true, version: 1 },
  { id: 'g-home', name: 'home automation', description: 'Lights, scenes, sensors', patterns: ['home_light_*', 'home_scene_run'], editable: true, version: 1 },
  { id: 'g-notes', name: 'notes', description: 'Note entries', patterns: ['notes::*'], editable: true, version: 1 },
  { id: 'g-workshop', name: 'workshop', description: 'Build jobs (upstream currently down)', patterns: ['workshop::*'], editable: false, version: 1 },
];

let clients: RmcpClient[] = [
  {
    id: 'c-1',
    clientId: 'cnx_reader',
    name: 'Reading assistant',
    registrationSource: 'operator',
    enabled: true,
    confidential: true,
    redirectUris: ['https://example.invalid/callback'],
    toolGroupIds: ['g-media', 'g-notes'],
    namespaces: ['media', 'notes'],
    createdAt: '2026-07-30T09:12:00Z',
    version: 3,
    editable: true,
  },
  {
    id: 'c-2',
    clientId: 'cnx_selfreg',
    name: 'Self-registered connector',
    registrationSource: 'dcr',
    enabled: false,
    confidential: false,
    redirectUris: ['https://example.invalid/oauth'],
    // Unscoped on purpose: a DCR client reaches nothing until an operator scopes it.
    toolGroupIds: [],
    namespaces: [],
    createdAt: '2026-08-02T18:40:00Z',
    version: 1,
    editable: true,
  },
  {
    id: 'c-3',
    clientId: 'cnx_workshop',
    name: 'Workshop console',
    registrationSource: 'operator',
    enabled: true,
    confidential: false,
    redirectUris: ['http://127.0.0.1:7777/callback'],
    toolGroupIds: ['g-workshop'],
    namespaces: ['workshop'],
    createdAt: '2026-07-11T11:00:00Z',
    version: 2,
    // Owned by another account in the fixture — exercises the read-only rendering path.
    editable: false,
  },
];

let sessions: RmcpSession[] = [
  { id: 's-1', accountName: 'operator', clientRowId: 'c-1', clientName: 'Reading assistant', scope: 'mcp', grantedAt: '2026-07-30T09:20:00Z', lastUsedAt: '2026-08-04T07:55:00Z', activeFamilies: 2, revokedAt: null },
  { id: 's-2', accountName: 'operator', clientRowId: 'c-1', clientName: 'Reading assistant', scope: 'mcp', grantedAt: '2026-08-01T14:02:00Z', lastUsedAt: null, activeFamilies: 1, revokedAt: null },
  { id: 's-3', accountName: 'workshop-owner', clientRowId: 'c-3', clientName: 'Workshop console', scope: 'mcp', grantedAt: '2026-07-12T08:00:00Z', lastUsedAt: '2026-07-28T19:31:00Z', activeFamilies: 0, revokedAt: '2026-07-29T10:00:00Z' },
];

let seq = 0;
function nextId(prefix: string): string {
  seq += 1;
  return `${prefix}-${seq}`;
}

function clientOr404(id: string, tool: RmcpToolName): RmcpClient {
  const found = clients.find(c => c.id === id);
  if (!found) throw new RmcpError('not_found', tool, 'client not found');
  return found;
}

/** The fixture server's ownership check — the same shape the real one has (RMCP-12), so the
 *  read-only rendering path is exercised offline instead of only in production. */
function assertEditable(client: RmcpClient, tool: RmcpToolName): void {
  if (!client.editable) throw new RmcpError('forbidden', tool, 'not owned by this account');
}

function resolveForClient(client: RmcpClient, limit?: number, offset?: number): RmcpResolvedScope {
  const assignedGroups = groups.filter(g => client.toolGroupIds.includes(g.id));
  const all: RmcpResolvedTool[] = [];
  const seen = new Set<string>();
  for (const g of assignedGroups) {
    for (const t of resolvePatterns(g.patterns, g.name)) {
      // Namespace scoping gates the mesh dimension: a tool from an upstream not assigned to the
      // client is invisible regardless of group matches (RMCP-07 rule 4).
      if (!client.namespaces.includes(t.namespace)) continue;
      if (seen.has(t.name)) continue;
      seen.add(t.name);
      all.push(t);
    }
  }
  all.sort((a, b) => a.name.localeCompare(b.name));
  const start = offset ?? 0;
  const end = limit === undefined ? all.length : start + limit;
  return {
    clientId: client.id,
    tools: all.slice(start, end),
    unavailableNamespaces: client.namespaces.filter(
      ns => FIXTURE_NAMESPACES.find(n => n.namespace === ns)?.available === false,
    ),
    truncated: end < all.length,
    catalogGeneration: 'fixture-1',
  };
}

const servers: RmcpServer[] = FIXTURE_NAMESPACES.map(ns => ({
  namespace: ns.namespace,
  ownerName: ns.namespace === 'workshop' ? 'workshop-owner' : 'operator',
  ownedByMe: ns.namespace !== 'workshop',
  available: ns.available,
  toolCount: ns.available ? ns.tools.length : null,
}));

// ── Dispatch ────────────────────────────────────────────────────────────────

/** Latency so loading states are real in mock mode rather than instantly resolved. */
function delay<T>(value: T): Promise<T> {
  return new Promise(resolve => setTimeout(() => resolve(value), 120));
}

/** The fixture server's `rmcp_*` dispatch. Mirrors the real envelope semantics: a refusal is a
 *  thrown `RmcpError` with the same kinds `callTool` would have produced from an `ok:false`
 *  envelope, so the panels take identical code paths in both modes. */
export function rmcpFixtureCall<T>(tool: RmcpToolName, args: Record<string, unknown>): Promise<T> {
  switch (tool) {
    case RMCP_TOOLS.clientList:
      return delay({ clients: clients.map(c => ({ ...c })) } as unknown as T);

    case RMCP_TOOLS.clientCreate: {
      const created: RmcpClient = {
        id: nextId('c'),
        clientId: `cnx_${String(args.name ?? 'connector').toLowerCase().replace(/[^a-z0-9]+/g, '_').replace(/^_|_$/g, '') || 'connector'}`,
        name: String(args.name ?? 'connector'),
        registrationSource: 'operator',
        enabled: true,
        confidential: args.confidential === true,
        redirectUris: (args.redirect_uris as string[] | undefined) ?? [],
        toolGroupIds: (args.tool_group_ids as string[] | undefined) ?? [],
        namespaces: (args.namespaces as string[] | undefined) ?? [],
        createdAt: new Date().toISOString(),
        version: 1,
        editable: true,
      };
      clients = [...clients, created];
      return delay({
        client: created,
        // Shown exactly once by the creation flow and never returned by any read tool. This is a
        // fixture value, not a credential: it is generated in-browser and authenticates nothing.
        clientSecret: created.confidential ? `fixture-secret-${created.id}-not-a-real-credential` : null,
      } as unknown as T);
    }

    case RMCP_TOOLS.clientUpdate: {
      const client = clientOr404(String(args.id), tool);
      assertEditable(client, tool);
      if (typeof args.version === 'number' && args.version !== client.version) {
        throw new RmcpError('conflict', tool, 'client was modified by another session');
      }
      const updated: RmcpClient = {
        ...client,
        enabled: typeof args.enabled === 'boolean' ? args.enabled : client.enabled,
        redirectUris: (args.redirect_uris as string[] | undefined) ?? client.redirectUris,
        toolGroupIds: (args.tool_group_ids as string[] | undefined) ?? client.toolGroupIds,
        namespaces: (args.namespaces as string[] | undefined) ?? client.namespaces,
        version: client.version + 1,
      };
      clients = clients.map(c => (c.id === updated.id ? updated : c));
      return delay({ client: updated } as unknown as T);
    }

    case RMCP_TOOLS.clientRevoke: {
      const client = clientOr404(String(args.id), tool);
      assertEditable(client, tool);
      clients = clients.filter(c => c.id !== client.id);
      sessions = sessions.map(s =>
        s.clientRowId === client.id && !s.revokedAt
          ? { ...s, revokedAt: new Date().toISOString(), activeFamilies: 0 }
          : s,
      );
      return delay(undefined as unknown as T);
    }

    case RMCP_TOOLS.clientResolve: {
      const client = clientOr404(String(args.id), tool);
      return delay(
        resolveForClient(client, args.limit as number | undefined, args.offset as number | undefined) as unknown as T,
      );
    }

    case RMCP_TOOLS.groupList:
      return delay({ groups: groups.map(g => ({ ...g })) } as unknown as T);

    case RMCP_TOOLS.groupCreate: {
      const patterns = (args.patterns as string[] | undefined) ?? [];
      const bad = patterns.map(p => ({ p, reason: patternRejection(p) })).filter(x => x.reason);
      if (bad.length) {
        throw new RmcpError('invalid', tool, 'invalid pattern', bad.map(b => `${b.p}: ${b.reason}`));
      }
      const created: RmcpToolGroup = {
        id: nextId('g'),
        name: String(args.name ?? ''),
        description: String(args.description ?? ''),
        patterns,
        editable: true,
        version: 1,
      };
      groups = [...groups, created];
      return delay({ group: created } as unknown as T);
    }

    case RMCP_TOOLS.groupUpdate: {
      const group = groups.find(g => g.id === String(args.id));
      if (!group) throw new RmcpError('not_found', tool, 'group not found');
      if (!group.editable) throw new RmcpError('forbidden', tool, 'not owned by this account');
      if (typeof args.version === 'number' && args.version !== group.version) {
        throw new RmcpError('conflict', tool, 'group was modified by another session');
      }
      const patterns = (args.patterns as string[] | undefined) ?? group.patterns;
      const bad = patterns.map(p => ({ p, reason: patternRejection(p) })).filter(x => x.reason);
      if (bad.length) {
        throw new RmcpError('invalid', tool, 'invalid pattern', bad.map(b => `${b.p}: ${b.reason}`));
      }
      const updated: RmcpToolGroup = {
        ...group,
        name: args.name === undefined ? group.name : String(args.name),
        description: args.description === undefined ? group.description : String(args.description),
        patterns,
        version: group.version + 1,
      };
      groups = groups.map(g => (g.id === updated.id ? updated : g));
      return delay({ group: updated } as unknown as T);
    }

    case RMCP_TOOLS.groupPreview: {
      const patterns = (args.patterns as string[] | undefined) ?? [];
      const invalid = patterns
        .map(p => ({ pattern: p, reason: patternRejection(p) }))
        .filter((x): x is { pattern: string; reason: string } => x.reason !== null);
      const matched = resolvePatterns(patterns, 'preview');
      const limit = (args.limit as number | undefined) ?? matched.length;
      return delay({
        tools: matched.slice(0, limit),
        invalidPatterns: invalid,
        truncated: matched.length > limit,
      } as unknown as T);
    }

    case RMCP_TOOLS.serverOwnerList:
      return delay({ servers: servers.map(s => ({ ...s })) } as unknown as T);

    case RMCP_TOOLS.sessionList: {
      const filter = args.client_id as string | undefined;
      return delay({
        sessions: sessions.filter(s => !filter || s.clientRowId === filter).map(s => ({ ...s })),
      } as unknown as T);
    }

    case RMCP_TOOLS.sessionRevoke: {
      const now = new Date().toISOString();
      const sessionId = args.session_id as string | undefined;
      const clientRowId = args.client_id as string | undefined;
      sessions = sessions.map(s => {
        const hit = sessionId ? s.id === sessionId : clientRowId ? s.clientRowId === clientRowId : false;
        return hit && !s.revokedAt ? { ...s, revokedAt: now, activeFamilies: 0 } : s;
      });
      return delay(undefined as unknown as T);
    }

    default:
      // An unmapped tool reads as "not deployed", the same as the live server's answer for a
      // tool it does not register — never as a success.
      throw new RmcpError('tool_unavailable', tool, `${tool} is not available in fixture mode`);
  }
}
