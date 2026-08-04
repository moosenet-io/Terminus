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
//  3. **It is at least as strict as the real server, never laxer.** Review round 1 caught the
//     opposite: an early version listed every session and skipped ownership on resolve/revoke. A
//     mock that is more permissive than the server trains the UI against a contract that does not
//     exist, and it is the surface the delegated-owner tests run against — so the one check that
//     matters most (a delegated owner cannot see or touch another owner's objects) could not be
//     exercised at all. Ownership is now enforced here on every read and every write.
//
// THE FIXTURE PRINCIPAL is a DELEGATED OWNER, not the operator: it owns the `media`, `home`,
// `workshop` and `notes` namespaces and the clients/groups built on them, and owns neither the
// `studio` namespace nor the client and group behind it. Modelling it that way is what makes the
// cross-owner refusals testable; an operator principal would own everything and prove nothing.
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

/** Identifiable content, asserted ABSENT from every production asset by
 *  `scripts/assert-http-bundle.mjs`. It is referenced in a thrown message below so a minifier
 *  cannot drop it while keeping the module: if the fixture ships, this string ships with it. */
export const RMCP_FIXTURE_MARKER = 'rmcp-fixture-server-must-never-ship';


// ── Fixture catalog ─────────────────────────────────────────────────────────
// A merged, namespaced catalog shaped like the real one: a few namespaces, one of them down.

const FIXTURE_NAMESPACES: { namespace: string; available: boolean; ownedByMe: boolean; tools: string[] }[] = [
  {
    namespace: 'media',
    available: true,
    ownedByMe: true,
    tools: ['media_search', 'media_play', 'media_queue_add', 'media_library_scan', 'media_stats'],
  },
  {
    namespace: 'home',
    available: true,
    ownedByMe: true,
    tools: ['home_light_set', 'home_scene_run', 'home_sensor_read', 'home_thermostat_set'],
  },
  {
    // Owned by the fixture principal but currently DOWN — the "scoped to an unavailable upstream"
    // state, which must read as a condition of the mesh, not as an error or a refusal.
    namespace: 'workshop',
    available: false,
    ownedByMe: true,
    tools: ['workshop_job_list', 'workshop_job_start'],
  },
  {
    namespace: 'notes',
    available: true,
    ownedByMe: true,
    // Padded so the resolved preview's paging is exercisable without a live 400-tool catalog.
    tools: Array.from({ length: 60 }, (_, i) => `notes_entry_${String(i + 1).padStart(3, '0')}`),
  },
  {
    // Someone ELSE's namespace: visible (its tools are in the merged catalog either way) but not
    // assignable by this principal. Scoping a client to it is refused at write, which is the
    // spec's headline delegated-owner test.
    namespace: 'studio',
    available: true,
    ownedByMe: false,
    tools: ['studio_render', 'studio_asset_list'],
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
//
// Rows carry an `owner` the wire types do not have: the real store knows who owns each row and
// answers accordingly, so the fixture has to know too. It is stripped from every response —
// leaking "this belongs to someone else" would itself be the disclosure the model forbids.

type Owner = 'me' | 'other';
type FixtureClient = RmcpClient & { owner: Owner };
type FixtureGroup = RmcpToolGroup & { owner: Owner };

/** Drop the fixture-only ownership field before answering. */
function wire<T extends { owner: Owner }>(row: T): Omit<T, 'owner'> {
  const { owner: _owner, ...rest } = row;
  return rest;
}

let groups: FixtureGroup[] = [
  { id: 'g-media', name: 'media', description: 'Library search and playback', patterns: ['media::*'], editable: true, version: 1, owner: 'me' },
  { id: 'g-home', name: 'home automation', description: 'Lights, scenes, sensors', patterns: ['home_light_*', 'home_scene_run'], editable: true, version: 1, owner: 'me' },
  { id: 'g-notes', name: 'notes', description: 'Note entries', patterns: ['notes::*'], editable: true, version: 1, owner: 'me' },
  { id: 'g-workshop', name: 'workshop', description: 'Build jobs (upstream currently down)', patterns: ['workshop::*'], editable: true, version: 1, owner: 'me' },
  // Another owner's group. Never returned by `rmcp_group_list` for this principal — enumeration
  // is itself a disclosure — and refused on direct access by id.
  { id: 'g-studio', name: 'studio', description: 'Rendering', patterns: ['studio::*'], editable: false, version: 1, owner: 'other' },
];

let clients: FixtureClient[] = [
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
    owner: 'me',
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
    owner: 'me',
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
    editable: true,
    owner: 'me',
  },
  {
    // Another owner's connector. Invisible to every read this principal makes, and refused —
    // not merely hidden — on any direct call naming its id.
    id: 'c-4',
    clientId: 'cnx_studio',
    name: 'Studio console',
    registrationSource: 'operator',
    enabled: true,
    confidential: false,
    redirectUris: ['https://example.invalid/studio'],
    toolGroupIds: ['g-studio'],
    namespaces: ['studio'],
    createdAt: '2026-06-02T08:00:00Z',
    version: 1,
    editable: false,
    owner: 'other',
  },
];

let sessions: RmcpSession[] = [
  { id: 's-1', accountName: 'delegated-owner', clientRowId: 'c-1', clientName: 'Reading assistant', scope: 'mcp', grantedAt: '2026-07-30T09:20:00Z', lastUsedAt: '2026-08-04T07:55:00Z', activeFamilies: 2, revokedAt: null },
  { id: 's-2', accountName: 'delegated-owner', clientRowId: 'c-1', clientName: 'Reading assistant', scope: 'mcp', grantedAt: '2026-08-01T14:02:00Z', lastUsedAt: null, activeFamilies: 1, revokedAt: null },
  { id: 's-3', accountName: 'delegated-owner', clientRowId: 'c-3', clientName: 'Workshop console', scope: 'mcp', grantedAt: '2026-07-12T08:00:00Z', lastUsedAt: '2026-07-28T19:31:00Z', activeFamilies: 1, revokedAt: null },
  // Another owner's session. Must never appear in a list this principal makes, and must not be
  // revocable by id — a revoke is a read of "does this exist" as much as a write.
  { id: 's-4', accountName: 'studio-owner', clientRowId: 'c-4', clientName: 'Studio console', scope: 'mcp', grantedAt: '2026-06-03T08:00:00Z', lastUsedAt: '2026-08-01T10:00:00Z', activeFamilies: 1, revokedAt: null },
];

let seq = 0;
function nextId(prefix: string): string {
  seq += 1;
  return `${prefix}-${seq}`;
}

function clientOr404(id: string, tool: RmcpToolName): FixtureClient {
  const found = clients.find(c => c.id === id);
  if (!found) throw new RmcpError('not_found', tool, 'client not found');
  // Ownership is checked on the READ path too, not only before a write. A resolve or a revoke
  // that names another owner's client id must be REFUSED, not merely absent from a list — the
  // UI's hiding is a courtesy, and a fixture that only hid would let a UI-only "enforcement"
  // pass its tests. `forbidden` (rather than `not_found`) matches how the real store audits the
  // attempt; a deployment that prefers strict non-disclosure may answer `not_found` instead, and
  // the UI treats both as "you cannot have this".
  if (found.owner !== 'me') throw new RmcpError('forbidden', tool, 'not owned by this account');
  return found;
}

/** Every namespace this principal may scope a client to (RMCP-12). */
function ownedNamespaces(): string[] {
  return FIXTURE_NAMESPACES.filter(n => n.ownedByMe).map(n => n.namespace);
}

/** The delegated-owner headline rule: a client may only be scoped to namespaces its editor owns.
 *  Enforced on the write, because that is where the real server enforces it — a UI that merely
 *  disables the checkbox has enforced nothing. */
function assertNamespacesOwned(namespaces: string[] | undefined, tool: RmcpToolName): void {
  if (!namespaces) return;
  const owned = new Set(ownedNamespaces());
  const foreign = namespaces.filter(ns => !owned.has(ns));
  if (foreign.length > 0) {
    throw new RmcpError('forbidden', tool, `not owned by this account: ${foreign.join(', ')}`);
  }
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
  ownerName: ns.ownedByMe ? 'delegated-owner' : 'studio-owner',
  ownedByMe: ns.ownedByMe,
  available: ns.available,
  toolCount: ns.available ? ns.tools.length : null,
}));

// ── Dispatch ────────────────────────────────────────────────────────────────

/** Latency so loading states are real in mock mode rather than instantly resolved. */
function delay<T>(value: T): Promise<T> {
  return new Promise(resolve => setTimeout(() => resolve(value), 120));
}

/** The fixture server's `rmcp_*` dispatch. Mirrors the real envelope semantics: a refusal is a
 *  rejected promise carrying an `RmcpError` of the same kind `callTool` would have produced from
 *  an `ok:false` envelope, so the panels take identical code paths in both modes.
 *
 *  `async` deliberately: a refusal must be a REJECTION, never a synchronous throw. Over a real
 *  transport there is no such thing as a synchronous failure, so a fixture that threw sync would
 *  be a second behaviour no caller has to handle against the live server — and callers written
 *  against it would quietly skip their error path. (Found by a test calling this directly rather
 *  than through `callTool`, whose own `async` wrapper had been masking the difference.) */
export async function rmcpFixtureCall<T>(tool: RmcpToolName, args: Record<string, unknown>): Promise<T> {
  switch (tool) {
    case RMCP_TOOLS.clientList:
      // Scoped read: another owner's clients are not listed at all. Enumeration is itself a
      // disclosure, so this filters rather than returning them marked non-editable.
      return delay({ clients: clients.filter(c => c.owner === 'me').map(wire) } as unknown as T);

    case RMCP_TOOLS.clientCreate: {
      assertNamespacesOwned(args.namespaces as string[] | undefined, tool);
      const created: FixtureClient = {
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
        owner: 'me',
      };
      clients = [...clients, created];
      return delay({
        client: wire(created),
        // Shown exactly once by the creation flow and never returned by any read tool. This is a
        // fixture value, not a credential: it is generated in-browser and authenticates nothing.
        clientSecret: created.confidential ? `fixture-secret-${created.id}-not-a-real-credential` : null,
      } as unknown as T);
    }

    case RMCP_TOOLS.clientUpdate: {
      const client = clientOr404(String(args.id), tool);
      assertNamespacesOwned(args.namespaces as string[] | undefined, tool);
      if (typeof args.version === 'number' && args.version !== client.version) {
        throw new RmcpError('conflict', tool, 'client was modified by another session');
      }
      const updated: FixtureClient = {
        ...client,
        enabled: typeof args.enabled === 'boolean' ? args.enabled : client.enabled,
        redirectUris: (args.redirect_uris as string[] | undefined) ?? client.redirectUris,
        toolGroupIds: (args.tool_group_ids as string[] | undefined) ?? client.toolGroupIds,
        namespaces: (args.namespaces as string[] | undefined) ?? client.namespaces,
        version: client.version + 1,
      };
      clients = clients.map(c => (c.id === updated.id ? updated : c));
      return delay({ client: wire(updated) } as unknown as T);
    }

    case RMCP_TOOLS.clientRevoke: {
      const client = clientOr404(String(args.id), tool);
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
      // Same scoped read as clients: another owner's groups are not enumerated.
      return delay({ groups: groups.filter(g => g.owner === 'me').map(wire) } as unknown as T);

    case RMCP_TOOLS.groupCreate: {
      const patterns = (args.patterns as string[] | undefined) ?? [];
      const bad = patterns.map(p => ({ p, reason: patternRejection(p) })).filter(x => x.reason);
      if (bad.length) {
        throw new RmcpError('invalid', tool, 'invalid pattern', bad.map(b => `${b.p}: ${b.reason}`));
      }
      const created: FixtureGroup = {
        id: nextId('g'),
        name: String(args.name ?? ''),
        description: String(args.description ?? ''),
        patterns,
        editable: true,
        version: 1,
        owner: 'me',
      };
      groups = [...groups, created];
      return delay({ group: wire(created) } as unknown as T);
    }

    case RMCP_TOOLS.groupUpdate: {
      const group = groups.find(g => g.id === String(args.id));
      if (!group) throw new RmcpError('not_found', tool, 'group not found');
      if (group.owner !== 'me') throw new RmcpError('forbidden', tool, 'not owned by this account');
      if (typeof args.version === 'number' && args.version !== group.version) {
        throw new RmcpError('conflict', tool, 'group was modified by another session');
      }
      const patterns = (args.patterns as string[] | undefined) ?? group.patterns;
      const bad = patterns.map(p => ({ p, reason: patternRejection(p) })).filter(x => x.reason);
      if (bad.length) {
        throw new RmcpError('invalid', tool, 'invalid pattern', bad.map(b => `${b.p}: ${b.reason}`));
      }
      const updated: FixtureGroup = {
        ...group,
        name: args.name === undefined ? group.name : String(args.name),
        description: args.description === undefined ? group.description : String(args.description),
        patterns,
        version: group.version + 1,
      };
      groups = groups.map(g => (g.id === updated.id ? updated : g));
      return delay({ group: wire(updated) } as unknown as T);
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
      // A named client is authorized first (so asking about someone else's is REFUSED, not
      // silently empty — an empty answer and a refusal say different things, and only one of
      // them is honest). An unfiltered list is scoped to this principal's own clients.
      if (filter) clientOr404(filter, tool);
      const mine = new Set(clients.filter(c => c.owner === 'me').map(c => c.id));
      return delay({
        sessions: sessions
          .filter(s => (filter ? s.clientRowId === filter : mine.has(s.clientRowId)))
          .map(s => ({ ...s })),
      } as unknown as T);
    }

    case RMCP_TOOLS.sessionRevoke: {
      const now = new Date().toISOString();
      const sessionId = args.session_id as string | undefined;
      const clientRowId = args.client_id as string | undefined;
      // A revoke naming NOTHING is refused, not answered with a cheerful success (review round 2).
      // "Matched no rows" and "you never said what to revoke" are different facts, and reporting
      // the second as the first tells an operator that access was cut when nothing was touched —
      // after which they stop looking. The fixture enforces this itself precisely because it is a
      // SERVER boundary: the caller's type signature is not a control the server may rely on.
      if (!sessionId && !clientRowId) {
        throw new RmcpError('invalid', tool, 'a revoke must name a session_id or a client_id');
      }
      // Authorize the target before touching anything: a revoke names an object, so an
      // unauthorized revoke is both a write attempt AND an existence oracle.
      if (clientRowId) clientOr404(clientRowId, tool);
      if (sessionId) {
        const target = sessions.find(s => s.id === sessionId);
        if (!target) throw new RmcpError('not_found', tool, 'session not found');
        clientOr404(target.clientRowId, tool);
      }
      sessions = sessions.map(s => {
        const hit = sessionId ? s.id === sessionId : clientRowId ? s.clientRowId === clientRowId : false;
        return hit && !s.revokedAt ? { ...s, revokedAt: now, activeFamilies: 0 } : s;
      });
      return delay(undefined as unknown as T);
    }

    default:
      // An unmapped tool reads as "not deployed", the same as the live server's answer for a
      // tool it does not register — never as a success.
      throw new RmcpError('tool_unavailable', tool, `${tool} is not available in ${RMCP_FIXTURE_MARKER}`);
  }
}
