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

/** Whose row it is. `null` (namespaces only) means UNCLAIMED — no ownership row exists, which
 *  the real store treats as "attachable by nobody", not "attachable by anyone". */
type Owner = 'me' | 'other';


// ── Fixture catalog ─────────────────────────────────────────────────────────
// A merged, namespaced catalog shaped like the real one: a few namespaces, one of them down.

const FIXTURE_NAMESPACES: { namespace: string; available: boolean; owner: Owner | null; tools: string[] }[] = [
  {
    namespace: 'media',
    available: true,
    owner: 'me',
    tools: ['media_search', 'media_play', 'media_queue_add', 'media_library_scan', 'media_stats'],
  },
  {
    namespace: 'home',
    available: true,
    owner: 'me',
    tools: ['home_light_set', 'home_scene_run', 'home_sensor_read', 'home_thermostat_set'],
  },
  {
    // Mine, but the upstream is DOWN — a condition of the mesh, not a refusal.
    namespace: 'workshop',
    available: false,
    owner: 'me',
    tools: ['workshop_job_list', 'workshop_job_start'],
  },
  {
    namespace: 'notes',
    available: true,
    owner: 'me',
    // Padded so the resolved preview's paging is exercisable without a live 400-tool catalog.
    tools: Array.from({ length: 60 }, (_, i) => `notes_entry_${String(i + 1).padStart(3, '0')}`),
  },
  {
    // Someone ELSE's namespace: visible, not assignable by this principal.
    namespace: 'studio',
    available: true,
    owner: 'other',
    tools: ['studio_render', 'studio_asset_list'],
  },
  {
    // UNCLAIMED — no row in `rmcp_server_owner` at all. The real store refuses to attach it and
    // resolves it to nothing (`client_namespaces` INNER JOINs the ownership table): "nobody has
    // claimed this server" must never read as "everyone may reach it". A fixture that allowed it
    // would teach the UI the opposite of the rule.
    namespace: 'lab',
    available: true,
    owner: null,
    tools: ['lab_run', 'lab_status'],
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
  // A group that WAS this principal's when c-5 was scoped to it and has since been transferred
  // away. The scope row survives the ownership that justified it — which is exactly why the real
  // store re-checks `c.owner_account_id = g.owner_account_id` on the READ path and not only at
  // write time. It resolves to nothing now.
  { id: 'g-legacy', name: 'legacy media', description: 'Transferred to another owner', patterns: ['media::*'], editable: false, version: 1, owner: 'other' },
  { id: 'g-lab', name: 'lab', description: 'Lab tools', patterns: ['lab::*'], editable: true, version: 1, owner: 'me' },
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
  {
    // Scoped, but DISABLED. The real store's scope reads both require `NOT c.disabled`, so this
    // resolves to nothing — the preview must not show a fabricated grant for a client the rest of
    // the UI simultaneously describes as denied.
    id: 'c-5',
    clientId: 'cnx_suspended',
    name: 'Suspended assistant',
    registrationSource: 'operator',
    enabled: false,
    confidential: false,
    redirectUris: ['https://example.invalid/suspended'],
    toolGroupIds: ['g-media'],
    namespaces: ['media'],
    createdAt: '2026-07-20T09:00:00Z',
    version: 1,
    editable: true,
    owner: 'me',
  },
  {
    // Enabled, correctly scoped — but its only group was transferred to another owner after the
    // assignment. Read-time ownership re-check ⇒ resolves to nothing.
    id: 'c-6',
    clientId: 'cnx_transferred',
    name: 'Transferred-group console',
    registrationSource: 'operator',
    enabled: true,
    confidential: false,
    redirectUris: ['https://example.invalid/transferred'],
    toolGroupIds: ['g-legacy'],
    namespaces: ['media'],
    createdAt: '2026-07-21T09:00:00Z',
    version: 1,
    editable: true,
    owner: 'me',
  },
  {
    // Enabled, group owned by this principal, matching patterns — but the namespace has NO owner
    // (delegation cleared, or never granted). Read-time ownership re-check ⇒ resolves to nothing.
    id: 'c-7',
    clientId: 'cnx_unclaimed',
    name: 'Unclaimed-server console',
    registrationSource: 'operator',
    enabled: true,
    confidential: false,
    redirectUris: ['https://example.invalid/unclaimed'],
    toolGroupIds: ['g-lab'],
    namespaces: ['lab'],
    createdAt: '2026-07-22T09:00:00Z',
    version: 1,
    editable: true,
    owner: 'me',
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
  // naming another owner's client must be REFUSED, not merely absent from a list — the UI's
  // hiding is a courtesy, and a fixture that only hid would let a UI-only "enforcement" pass its
  // tests.
  //
  // The KIND is `not_found`, matching the merged store verbatim ("Same answer for 'no such
  // client' and 'not yours': distinguishing them would confirm the existence of another account's
  // client"). Round 2 of this item picked `forbidden` on my own judgement; with the real store
  // now merged there is an authority, and it says the two answers must be indistinguishable — a
  // `forbidden` that a `not_found` would not have produced IS the enumeration oracle.
  if (found.owner !== 'me') throw new RmcpError('not_found', tool, 'no such client for this account');
  return found;
}

/** Namespaces this principal may attach — owned BY THIS PRINCIPAL. An unclaimed namespace is not
 *  in this set, mirroring `set_client_namespaces`' INNER JOIN on `rmcp_server_owner`. */
function ownedNamespaces(): string[] {
  return FIXTURE_NAMESPACES.filter(n => n.owner === 'me').map(n => n.namespace);
}

/**
 * Mirrors `OauthStore::set_client_namespaces`' ownership check. An UNOWNED namespace is REFUSED,
 * not allowed — the store's own words: "nobody has claimed this server" must mean "no delegated
 * owner may attach it", never "it is free for anyone". The refusal is deliberately unspecific
 * about WHICH namespace failed, exactly as the store is, so the error is not an enumeration
 * oracle for another account's servers.
 */
function assertNamespacesAssignable(namespaces: string[] | undefined, tool: RmcpToolName): void {
  if (!namespaces) return;
  const owned = new Set(ownedNamespaces());
  if (namespaces.some(ns => !owned.has(ns))) {
    throw new RmcpError('invalid', tool, 'one or more servers are not owned by this account');
  }
}

/**
 * Mirrors `OauthStore::set_client_tool_groups`' ownership check — the symmetric rule that was
 * missing here entirely: every assigned group must belong to the actor. Same unspecific error,
 * same reason.
 */
function assertGroupsAssignable(groupIds: string[] | undefined, tool: RmcpToolName): void {
  if (!groupIds) return;
  const owned = new Set(groups.filter(g => g.owner === 'me').map(g => g.id));
  if (groupIds.some(id => !owned.has(id))) {
    throw new RmcpError('invalid', tool, 'one or more tool groups do not belong to this account');
  }
}

/**
 * The fixture's effective-scope resolution, mirrored PREDICATE FOR PREDICATE from the merged
 * `OauthStore::client_tool_groups` / `client_namespaces` (`src/oauth/store.rs` on main). Each
 * clause below cites the SQL it stands in for, because the value of this function is entirely in
 * agreeing with that one:
 *
 *   groups:     JOIN rmcp_client c ON c.id = s.client_id AND NOT c.disabled
 *                                  AND c.owner_account_id = g.owner_account_id
 *   namespaces: JOIN rmcp_client c ON c.id = s.client_id AND NOT c.disabled
 *               JOIN rmcp_server_owner o ON o.namespace = s.namespace
 *                                       AND o.owner_account_id = c.owner_account_id
 *
 * The through-line in all three added clauses: **a write-time check is point-in-time, and any
 * revocable authority has to be re-derived on READ.** Ownership can be transferred and a
 * delegation can be cleared, but the `rmcp_client_scope` / `rmcp_client_server` row outlives
 * both — so a resolver that trusted the write would keep reporting a grant that no longer
 * exists. That lesson cost RMCP-01 two review rounds (10 and 11) and recurred in RMCP-06; a
 * fixture that did not embody it would quietly re-teach the mistake to whoever reads it next.
 */
function resolveForClient(client: FixtureClient, limit?: number, offset?: number): RmcpResolvedScope {
  // `NOT c.disabled`, on BOTH scope reads. A disabled client resolves to nothing — not to a
  // preview of what it would reach if re-enabled, which would be a fabricated grant shown next
  // to a "disabled" badge.
  const enabled = client.enabled;

  // `c.owner_account_id = g.owner_account_id`, re-checked at read time: a group transferred to
  // another owner stops resolving even though the assignment row remains.
  const assignedGroups = enabled
    ? groups.filter(g => client.toolGroupIds.includes(g.id) && g.owner === client.owner)
    : [];

  // The `rmcp_server_owner` join, re-checked at read time: the namespace must still be owned, and
  // owned BY THIS CLIENT'S OWNER. An unclaimed namespace (no ownership row) resolves to nothing —
  // "nobody has claimed this server" is not "everyone may reach it".
  const effectiveNamespaces = enabled
    ? client.namespaces.filter(ns => {
        const row = FIXTURE_NAMESPACES.find(n => n.namespace === ns);
        return row !== undefined && row.owner !== null && row.owner === client.owner;
      })
    : [];

  const all: RmcpResolvedTool[] = [];
  const seen = new Set<string>();
  for (const g of assignedGroups) {
    for (const t of resolvePatterns(g.patterns, g.name)) {
      // Namespace scoping gates the mesh dimension: a tool from an upstream not in the client's
      // EFFECTIVE namespaces is invisible regardless of group matches (RMCP-07 rule 4).
      if (!effectiveNamespaces.includes(t.namespace)) continue;
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
    // Only namespaces that actually resolve can be "in scope but down" — one that fails the
    // ownership predicate is not unavailable, it is not in scope at all.
    unavailableNamespaces: effectiveNamespaces.filter(
      ns => FIXTURE_NAMESPACES.find(n => n.namespace === ns)?.available === false,
    ),
    truncated: end < all.length,
    catalogGeneration: 'fixture-1',
  };
}

const servers: RmcpServer[] = FIXTURE_NAMESPACES.map(ns => ({
  namespace: ns.namespace,
  // `null` distinguishes UNCLAIMED from "someone else's" — different facts with different
  // remedies (claim it vs ask its owner), and the UI says which.
  ownerName: ns.owner === null ? null : ns.owner === 'me' ? 'delegated-owner' : 'studio-owner',
  ownedByMe: ns.owner === 'me',
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
      assertNamespacesAssignable(args.namespaces as string[] | undefined, tool);
      assertGroupsAssignable(args.tool_group_ids as string[] | undefined, tool);
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
      assertNamespacesAssignable(args.namespaces as string[] | undefined, tool);
      assertGroupsAssignable(args.tool_group_ids as string[] | undefined, tool);
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
      // Same non-oracle answer as the client path above.
      if (group.owner !== 'me') throw new RmcpError('not_found', tool, 'no such group for this account');
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
      // `RMCP_SELECTOR_RULE`: exactly one selector. Both refusals below are the same rule, and the
      // fixture enforces it itself precisely because it is a SERVER boundary — the caller's type
      // signature is not a control the server may rely on.
      //
      // BOTH (round 4): refused rather than resolved by precedence. Picking one and reporting
      // success tells an operator that two revocations happened when one did, and which one they
      // got would depend on the order of the tests below rather than on anything they can see.
      if (sessionId && clientRowId) {
        throw new RmcpError('invalid', tool, 'a revoke must name either a session_id or a client_id, not both');
      }
      // NEITHER (round 2): "matched no rows" and "you never said what to revoke" are different
      // facts, and reporting the second as the first convinces an operator access was cut when
      // nothing was touched — after which they stop looking.
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
