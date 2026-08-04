// RMCP-13 (TERM-624): the connector data layer, exercised against the fixture server.
//
// In a unit-test process there is no `window`, so `resolveMode()` is `mock` and every call here
// lands on `rmcpFixtures.ts` — the one mocked boundary. That is exactly what makes these tests
// meaningful: the PANELS call the same functions, take the same code paths, and would take them
// against the live tools with nothing changed but the mode.
import { describe, it, expect } from 'vitest';
import {
  createClient,
  createGroup,
  listClients,
  listGroups,
  listServers,
  listSessions,
  previewGroup,
  resolveClientScope,
  revokeSessions,
  updateClient,
  updateGroup,
  describeRmcpError,
  RmcpError,
} from './rmcpClient';

// ── The line that must not be crossed ────────────────────────────────────────
//
// The resolved preview is only trustworthy because it is the SERVER's answer. If a panel ever
// imported the fixture matcher (or grew one of its own), the preview would become a plausible
// guess that agrees with reality right up until it doesn't. This scans the sources the way the
// existing fetch-exclusivity guard does.
// @ts-expect-error -- import.meta.glob has no ambient type in this project (see aggregationClient.sessions.test.ts)
const RAW_SOURCES: Record<string, string> = import.meta.glob('/src/**/*.{ts,tsx}', { query: '?raw', import: 'default', eager: true });

describe('no UI module resolves scope locally', () => {
  it('nothing under src/panels or src/pages imports the fixture server', () => {
    const offenders = Object.entries(RAW_SOURCES)
      .filter(([path]) => path.startsWith('/src/panels/') || path.startsWith('/src/pages/'))
      .filter(([, text]) => /rmcpFixtures/.test(text))
      .map(([path]) => path);
    expect(offenders).toEqual([]);
  });

  it('only the client references the fixture module, and only behind the build-time guard', () => {
    const referencing = Object.entries(RAW_SOURCES)
      .filter(([, text]) => /rmcpFixtures'/.test(text))
      .map(([path]) => path)
      .sort();
    expect(referencing).toEqual(['/src/lib/rmcpClient.ts']);

    // Structural, not documentary (review round 1): the only reference is a dynamic import
    // guarded by a literal `!import.meta.env.PROD`, which Vite folds to `false` in a production
    // build so the module never enters that bundle's graph. A top-level `import … from
    // './rmcpFixtures'` would defeat that, so assert there isn't one.
    const client = RAW_SOURCES['/src/lib/rmcpClient.ts'];
    expect(client).not.toMatch(/^import .*rmcpFixtures/m);
    expect(client).toMatch(/!import\.meta\.env\.PROD && resolveMode\(\) === 'mock'/);
    expect(client).toMatch(/await import\('\.\/rmcpFixtures'\)/);
  });
});

describe('client list + resolved scope', () => {
  it('lists clients with their scoping assignments', async () => {
    const clients = await listClients();
    expect(clients.length).toBeGreaterThan(0);
    const reader = clients.find(c => c.name === 'Reading assistant');
    expect(reader?.toolGroupIds.length).toBeGreaterThan(0);
    expect(reader?.namespaces).toContain('media');
  });

  it('resolves a client to concrete tools, each carrying the group and pattern that matched', async () => {
    const clients = await listClients();
    const reader = clients.find(c => c.name === 'Reading assistant')!;
    const scope = await resolveClientScope(reader.id);
    expect(scope.tools.length).toBeGreaterThan(0);
    for (const tool of scope.tools) {
      expect(reader.namespaces).toContain(tool.namespace);
      expect(tool.matchedGroup).not.toBe('');
      expect(tool.matchedPattern).not.toBe('');
    }
  });

  it('gates the mesh dimension: a group match outside the assigned namespaces is not reachable', async () => {
    const clients = await listClients();
    const reader = clients.find(c => c.name === 'Reading assistant')!;
    const scope = await resolveClientScope(reader.id);
    expect(scope.tools.some(t => t.namespace === 'home')).toBe(false);
  });

  it('reports an assigned-but-down namespace as unavailable rather than failing', async () => {
    const clients = await listClients();
    const workshop = clients.find(c => c.name === 'Workshop console')!;
    const scope = await resolveClientScope(workshop.id);
    expect(scope.unavailableNamespaces).toContain('workshop');
    expect(scope.tools.every(t => t.available === false)).toBe(true);
  });

  it('pages a large resolution and reports truncation', async () => {
    const clients = await listClients();
    const reader = clients.find(c => c.name === 'Reading assistant')!;
    const first = await resolveClientScope(reader.id, { limit: 5, offset: 0 });
    expect(first.tools).toHaveLength(5);
    expect(first.truncated).toBe(true);
    const second = await resolveClientScope(reader.id, { limit: 5, offset: 5 });
    expect(second.tools.map(t => t.name)).not.toEqual(first.tools.map(t => t.name));
  });
});

describe('scoping edits round-trip, and the preview follows them', () => {
  it('saving a namespace change changes what the server says the client reaches', async () => {
    const before = (await listClients()).find(c => c.name === 'Reading assistant')!;
    const beforeScope = await resolveClientScope(before.id);
    expect(beforeScope.tools.length).toBeGreaterThan(0);

    const after = await updateClient({
      id: before.id,
      version: before.version,
      namespaces: [],
    });
    expect(after.version).toBe(before.version + 1);
    // Absence is denial: dropping every namespace leaves the groups intact and the reach empty.
    const afterScope = await resolveClientScope(after.id);
    expect(afterScope.tools).toEqual([]);

    // Restore, so the rest of the suite sees the original fixture.
    await updateClient({ id: after.id, version: after.version, namespaces: before.namespaces });
  });

  it('a stale version is refused as a conflict, never applied', async () => {
    const client = (await listClients()).find(c => c.name === 'Reading assistant')!;
    await updateClient({ id: client.id, version: client.version, enabled: true });
    await expect(updateClient({ id: client.id, version: client.version, enabled: false })).rejects.toMatchObject({
      kind: 'conflict',
    });
    const unchanged = (await listClients()).find(c => c.id === client.id)!;
    expect(unchanged.enabled).toBe(true);
  });

  it('refuses scoping a client to a namespace this session does not own — the headline rule', async () => {
    const client = (await listClients()).find(c => c.name === 'Reading assistant')!;
    await expect(
      updateClient({ id: client.id, version: client.version, namespaces: ['studio'] }),
    ).rejects.toMatchObject({ kind: 'forbidden' });
    // And the refusal is a refusal, not a partial apply.
    const after = (await listClients()).find(c => c.id === client.id)!;
    expect(after.namespaces).not.toContain('studio');
    expect(after.version).toBe(client.version);
  });

  it('refuses creating a client scoped to a namespace this session does not own', async () => {
    await expect(
      createClient({ name: 'Sneaky', redirectUris: [], confidential: false, toolGroupIds: [], namespaces: ['studio'] }),
    ).rejects.toMatchObject({ kind: 'forbidden' });
  });
});

describe('client creation shows the secret exactly once', () => {
  it('returns a secret on create and never again from any read', async () => {
    const created = await createClient({
      name: 'Test connector',
      redirectUris: ['https://example.invalid/cb'],
      confidential: true,
      toolGroupIds: [],
      namespaces: [],
    });
    expect(created.clientSecret).toBeTruthy();

    const listed = (await listClients()).find(c => c.id === created.client.id)!;
    // The read model has no secret field at all — there is nowhere for it to come back.
    expect(Object.keys(listed)).not.toContain('clientSecret');
    expect(JSON.stringify(listed)).not.toContain(created.clientSecret as string);
  });

  it('mints no secret for a public client', async () => {
    const created = await createClient({
      name: 'Public connector',
      redirectUris: ['https://example.invalid/cb'],
      confidential: false,
      toolGroupIds: [],
      namespaces: [],
    });
    expect(created.clientSecret).toBeNull();
  });
});

describe('group preview is the server’s match', () => {
  it('resolves the three supported pattern forms', async () => {
    const exact = await previewGroup(['media_search']);
    expect(exact.tools.map(t => t.name)).toEqual(['media::media_search']);

    const prefix = await previewGroup(['home::home_light_*']);
    expect(prefix.tools.length).toBeGreaterThan(0);
    expect(prefix.tools.every(t => t.namespace === 'home')).toBe(true);

    const ns = await previewGroup(['media::*']);
    expect(ns.tools.every(t => t.namespace === 'media')).toBe(true);
  });

  it('returns the empty set for a zero-match pattern — never everything', async () => {
    const preview = await previewGroup(['nothing_matches_this_*']);
    expect(preview.tools).toEqual([]);
  });

  it('names invalid patterns instead of matching on them', async () => {
    const preview = await previewGroup(['ok_*', 'ba*d']);
    expect(preview.invalidPatterns.map(p => p.pattern)).toEqual(['ba*d']);
  });

  it('caps a large preview and says so', async () => {
    const preview = await previewGroup(['notes::*'], 10);
    expect(preview.tools).toHaveLength(10);
    expect(preview.truncated).toBe(true);
  });

  it('rejects an invalid pattern at write time', async () => {
    await expect(createGroup({ name: 'bad', description: '', patterns: ['ba*d'] })).rejects.toMatchObject({
      kind: 'invalid',
    });
  });

  it('lists servers, distinguishing "down" from "not mine" — they are different problems', async () => {
    const servers = await listServers();
    const down = servers.find(s => s.namespace === 'workshop')!;
    expect(down.available).toBe(false);
    expect(down.ownedByMe).toBe(true);
    const foreign = servers.find(s => s.namespace === 'studio')!;
    expect(foreign.available).toBe(true);
    expect(foreign.ownedByMe).toBe(false);
  });
});

describe('sessions', () => {
  it('filters by client and revokes per row', async () => {
    const client = (await listClients()).find(c => c.name === 'Reading assistant')!;
    const sessions = await listSessions(client.id);
    expect(sessions.length).toBeGreaterThan(0);
    expect(sessions.every(s => s.clientRowId === client.id)).toBe(true);

    await revokeSessions({ sessionId: sessions[0].id });
    const after = await listSessions(client.id);
    // A revoked session stays visible, marked — the list is an audit surface, not just a control.
    expect(after).toHaveLength(sessions.length);
    expect(after.find(s => s.id === sessions[0].id)?.revokedAt).toBeTruthy();
  });

  it('revokes every session for one client', async () => {
    const client = (await listClients()).find(c => c.name === 'Reading assistant')!;
    await revokeSessions({ clientRowId: client.id });
    const after = await listSessions(client.id);
    expect(after.every(s => s.revokedAt !== null)).toBe(true);
  });
});

// ── Delegated ownership (RMCP-12) ────────────────────────────────────────────
//
// These run against the FIXTURE's own enforcement, deliberately. The UI hiding another owner's
// objects proves nothing — the question is whether the server refuses, and a mock laxer than the
// server would let a UI-only "enforcement" pass its tests (review round 1). The fixture principal
// is a delegated owner: it owns media/home/workshop/notes and neither `studio` nor the client,
// group, and session behind it.
describe('a delegated owner sees and touches only their own objects', () => {
  it('does not enumerate another owner\'s clients or groups — enumeration is disclosure', async () => {
    const clients = await listClients();
    expect(clients.find(c => c.name === 'Studio console')).toBeUndefined();
    const groups = await listGroups();
    expect(groups.find(g => g.name === 'studio')).toBeUndefined();
  });

  it('refuses to RESOLVE another owner\'s client, rather than just omitting it from a list', async () => {
    await expect(resolveClientScope('c-4')).rejects.toMatchObject({ kind: 'forbidden' });
  });

  it('refuses to edit or revoke another owner\'s client', async () => {
    await expect(updateClient({ id: 'c-4', version: 1, enabled: false })).rejects.toMatchObject({
      kind: 'forbidden',
    });
    await expect(updateGroup({ id: 'g-studio', version: 1, patterns: ['*'] })).rejects.toMatchObject({
      kind: 'forbidden',
    });
  });

  it('does not list another owner\'s sessions, and refuses one asked for by client id', async () => {
    const all = await listSessions();
    expect(all.find(s => s.clientRowId === 'c-4')).toBeUndefined();
    await expect(listSessions('c-4')).rejects.toMatchObject({ kind: 'forbidden' });
  });

  it('refuses to revoke another owner\'s session, by session id or by client id', async () => {
    await expect(revokeSessions({ sessionId: 's-4' })).rejects.toMatchObject({ kind: 'forbidden' });
    await expect(revokeSessions({ clientRowId: 'c-4' })).rejects.toMatchObject({ kind: 'forbidden' });
    // Still live: a refused revoke must not half-apply.
    expect((await listSessions()).find(s => s.id === 's-4')).toBeUndefined();
  });
});

describe('describeRmcpError', () => {
  it('explains a conflict as "reload, do not overwrite"', () => {
    const d = describeRmcpError(new RmcpError('conflict', 'rmcp_client_update', 'stale'));
    expect(d.kind).toBe('conflict');
    expect(d.message).toMatch(/Reload/i);
  });

  it('explains an undeployed tool as not-live rather than as a fault', () => {
    const d = describeRmcpError(new RmcpError('tool_unavailable', 'rmcp_client_list', 'nope'));
    expect(d.message).toMatch(/not live/i);
  });

  it('falls back to a generic message for a non-RmcpError', () => {
    expect(describeRmcpError(new Error('boom')).kind).toBe('error');
  });
});
