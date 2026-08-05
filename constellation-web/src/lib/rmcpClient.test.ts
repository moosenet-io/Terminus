// RMCP-13 (TERM-624): the connector data layer, exercised against the fixture server.
//
// In a unit-test process there is no `window`, so `resolveMode()` is `mock` and every call here
// lands on `rmcpFixtures.ts` — the one mocked boundary. That is exactly what makes these tests
// meaningful: the PANELS call the same functions, take the same code paths, and would take them
// against the live tools with nothing changed but the mode.
import { describe, it, expect } from 'vitest';
import {
  createAccount,
  listAccounts,
  setAccountDisabled,
  setAccountOperator,
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
//
// TWO TRAPS, both found by review round 2, both of the "a guard test quietly stops guarding"
// family — it still passes, so nobody looks:
//
//  1. **The needle must not appear literally in a scanned file.** A regex literal containing the
//     module name makes the scanning file itself a match. It happens that Vite's `import.meta.glob`
//     EXCLUDES the importing module from its own result (verified directly: a probe file's own path
//     is absent from its glob keys), so the round-2 version of this test was in fact reporting what
//     it claimed. But it was accidentally right: move the glob into a shared helper, or add a
//     second test file that names the module, and the result silently changes. A guard may not
//     depend on an implicit bundler behaviour nobody wrote down. The needle is therefore assembled
//     at RUNTIME, so it never appears as a literal in any scanned source.
//  2. **Tests are not shipped and legitimately name the module.** They are excluded explicitly
//     rather than by accident, which also removes any dependence on trap 1's self-exclusion.
//
// Non-vacuity is verified the same way the bundle assertion was: by temporarily adding a real
// violating import and watching this fail. See the commit message for what was observed.

// @ts-expect-error -- import.meta.glob has no ambient type in this project (see aggregationClient.sessions.test.ts)
const RAW_SOURCES: Record<string, string> = import.meta.glob('/src/**/*.{ts,tsx}', { query: '?raw', import: 'default', eager: true });

/** Assembled at runtime — see trap 1. `'rmcp' + 'Fixtures'`. */
const FIXTURE_MODULE = ['rmcp', 'Fix', 'tures'].join('');

/** Sources this guard is responsible for: everything the app ships, excluding test files. */
function scannedSources(): [string, string][] {
  return Object.entries(RAW_SOURCES).filter(([path]) => !/\.test\.tsx?$/.test(path));
}

describe('no UI module resolves scope locally', () => {
  it('the scan actually covers the files it claims to (self-test)', () => {
    // Guards against the silent-empty-scan failure: if the glob returned nothing, or excluded the
    // very files of interest, every assertion below would pass vacuously.
    const paths = scannedSources().map(([p]) => p);
    expect(paths).toContain('/src/lib/rmcpClient.ts');
    expect(paths).toContain('/src/pages/Connectors.tsx');
    expect(paths).toContain('/src/panels/connectors/ResolvedToolPreview.tsx');
    expect(paths.some(p => /\.test\.tsx?$/.test(p))).toBe(false);
  });

  it('nothing under src/panels or src/pages references the fixture server', () => {
    const offenders = scannedSources()
      .filter(([path]) => path.startsWith('/src/panels/') || path.startsWith('/src/pages/'))
      .filter(([, text]) => text.includes(FIXTURE_MODULE))
      .map(([path]) => path);
    expect(offenders).toEqual([]);
  });

  it('only the API client references the fixture module, and only behind the build-time guard', () => {
    const referencing = scannedSources()
      .filter(([path]) => path !== `/src/lib/${FIXTURE_MODULE}.ts`) // the module naming itself
      .filter(([, text]) => text.includes(FIXTURE_MODULE))
      .map(([path]) => path)
      .sort();
    expect(referencing).toEqual(['/src/lib/rmcpClient.ts']);

    // Structural, not documentary (review round 1): the only reference is a dynamic import
    // guarded by a literal `!import.meta.env.PROD`, which Vite folds to `false` in a production
    // build so the module never enters that bundle's graph. A top-level `import … from
    // './rmcpFixtures'` would defeat that, so assert there isn't one.
    const client = RAW_SOURCES['/src/lib/rmcpClient.ts'];
    expect(client).not.toMatch(new RegExp(`^import .*${FIXTURE_MODULE}`, 'm'));
    expect(client).toMatch(/!import\.meta\.env\.PROD && resolveMode\(\) === 'mock'/);
    expect(client).toMatch(new RegExp(`await import\\('\\./${FIXTURE_MODULE}'\\)`));
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

// ── Read-path predicates, mirrored from the merged `OauthStore` (src/oauth/store.rs) ─────────
//
// Each of these asserts a clause of the REAL store's scope queries, because the preview is only
// worth anything if it agrees with them. The shared lesson: a write-time check is point-in-time,
// so every revocable authority is re-derived on READ — the scope row outlives the ownership that
// justified it.
describe('the resolver re-derives authority on read, exactly as the store does', () => {
  it('a DISABLED client resolves to nothing, however well scoped (NOT c.disabled)', async () => {
    const suspended = (await listClients()).find(c => c.name === 'Suspended assistant')!;
    // Scoped on paper...
    expect(suspended.toolGroupIds).toContain('g-media');
    expect(suspended.namespaces).toContain('media');
    expect(suspended.enabled).toBe(false);
    // ...and reaching nothing in fact. A preview showing this client's would-be grant next to a
    // "disabled" badge would be a fabricated authorization answer.
    const scope = await resolveClientScope(suspended.id);
    expect(scope.tools).toEqual([]);
    expect(scope.unavailableNamespaces).toEqual([]);
  });

  it('a group transferred to another owner stops resolving (c.owner = g.owner, at read time)', async () => {
    const client = (await listClients()).find(c => c.name === 'Transferred-group console')!;
    // The assignment row survives — that is the whole point; only the ownership moved.
    expect(client.toolGroupIds).toContain('g-legacy');
    expect(client.enabled).toBe(true);
    expect(client.namespaces).toContain('media');
    expect((await resolveClientScope(client.id)).tools).toEqual([]);
  });

  it('an UNCLAIMED namespace resolves to nothing (the rmcp_server_owner join)', async () => {
    const client = (await listClients()).find(c => c.name === 'Unclaimed-server console')!;
    expect(client.namespaces).toEqual(['lab']);
    const servers = await listServers();
    expect(servers.find(s => s.namespace === 'lab')!.ownerName).toBeNull();
    // "Nobody has claimed this server" must never read as "everyone may reach it".
    expect((await resolveClientScope(client.id)).tools).toEqual([]);
  });

  it('refuses to ATTACH an unclaimed namespace too — read and write agree', async () => {
    const client = (await listClients()).find(c => c.name === 'Reading assistant')!;
    await expect(
      updateClient({ id: client.id, version: client.version, namespaces: ['lab'] }),
    ).rejects.toMatchObject({ kind: 'invalid' });
  });

  it('refuses to assign a tool group this account does not own', async () => {
    const client = (await listClients()).find(c => c.name === 'Reading assistant')!;
    await expect(
      updateClient({ id: client.id, version: client.version, toolGroupIds: ['g-studio'] }),
    ).rejects.toMatchObject({ kind: 'invalid' });
    await expect(
      createClient({
        // Accounts that resolve and are authorized, so the refusal below is unambiguously about
        // the borrowed GROUP and not about ownership of the connector itself (TERM-647).
        owner: 'delegated-owner',
        actor: 'delegated-owner',
        name: 'Borrowed',
        redirectUris: [],
        confidential: false,
        toolGroupIds: ['g-studio'],
        namespaces: [],
      }),
    ).rejects.toMatchObject({ kind: 'invalid' });
  });

  it('names neither the group nor the server that failed — not an enumeration oracle', async () => {
    const client = (await listClients()).find(c => c.name === 'Reading assistant')!;
    const err = await updateClient({ id: client.id, version: client.version, namespaces: ['studio'] }).catch(e => e);
    expect(err.message).not.toContain('studio');
    expect(err.message).toBe('one or more servers are not owned by this account');
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
    ).rejects.toMatchObject({ kind: 'invalid' });
    // And the refusal is a refusal, not a partial apply.
    const after = (await listClients()).find(c => c.id === client.id)!;
    expect(after.namespaces).not.toContain('studio');
    expect(after.version).toBe(client.version);
  });

  it('refuses creating a client scoped to a namespace this session does not own', async () => {
    await expect(
      createClient({
        owner: 'delegated-owner',
        actor: 'delegated-owner',
        name: 'Sneaky',
        redirectUris: [],
        confidential: false,
        toolGroupIds: [],
        namespaces: ['studio'],
      }),
    ).rejects.toMatchObject({ kind: 'invalid' });
  });
});

describe('client creation shows the secret exactly once', () => {
  it('returns a secret on create and never again from any read', async () => {
    const created = await createClient({
      owner: 'delegated-owner',
      actor: 'delegated-owner',
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
      owner: 'delegated-owner',
      actor: 'delegated-owner',
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
    // `not_found`, not `forbidden` — the merged store answers "no such client for this account"
    // precisely so the two are indistinguishable (see rmcpFixtures' clientOr404 note).
    await expect(resolveClientScope('c-4')).rejects.toMatchObject({ kind: 'not_found' });
  });

  it('refuses to edit or revoke another owner\'s client', async () => {
    await expect(updateClient({ id: 'c-4', version: 1, enabled: false })).rejects.toMatchObject({
      kind: 'not_found',
    });
    await expect(updateGroup({ id: 'g-studio', version: 1, patterns: ['*'] })).rejects.toMatchObject({
      kind: 'not_found',
    });
  });

  it('does not list another owner\'s sessions, and refuses one asked for by client id', async () => {
    const all = await listSessions();
    expect(all.find(s => s.clientRowId === 'c-4')).toBeUndefined();
    await expect(listSessions('c-4')).rejects.toMatchObject({ kind: 'not_found' });
  });

  it('refuses to revoke another owner\'s session, by session id or by client id', async () => {
    await expect(revokeSessions({ sessionId: 's-4' })).rejects.toMatchObject({ kind: 'not_found' });
    await expect(revokeSessions({ clientRowId: 'c-4' })).rejects.toMatchObject({ kind: 'not_found' });
    // Still live: a refused revoke must not half-apply.
    expect((await listSessions()).find(s => s.id === 's-4')).toBeUndefined();
  });
});

// ── Fixture/production parity (review round 5) ───────────────────────────────────────────────
//
// Both of these are the same failure: the mock being LAXER than the server it stands in for. A
// fixture that waves something through teaches the UI a contract that does not exist, and the UI
// is then written against the permissive branch.
describe('the concurrency check is fail-CLOSED', () => {
  it('refuses an update that states no version, and leaves the record untouched', async () => {
    const { rmcpFixtureCall } = await import('./rmcpFixtures');
    const before = (await listClients()).find(c => c.name === 'Reading assistant')!;

    // Straight at the server boundary: the typed wrapper requires `version`, so this is only
    // reachable from an untyped caller — which is exactly what a boundary is for.
    await expect(
      rmcpFixtureCall('rmcp_client_update', { id: before.id, enabled: false }),
    ).rejects.toMatchObject({ kind: 'invalid' });

    const after = (await listClients()).find(c => c.id === before.id)!;
    expect(after.enabled).toBe(before.enabled);
    // The version must NOT have been bumped: a refused write that still advanced the revision
    // would invalidate every other editor's in-flight form for a change that never happened.
    expect(after.version).toBe(before.version);
  });

  it('refuses a non-numeric version rather than ignoring it', async () => {
    const { rmcpFixtureCall } = await import('./rmcpFixtures');
    const client = (await listClients()).find(c => c.name === 'Reading assistant')!;
    for (const version of ['3', null, {}, 1.5, -1]) {
      await expect(
        rmcpFixtureCall('rmcp_client_update', { id: client.id, version, enabled: false }),
      ).rejects.toMatchObject({ kind: 'invalid' });
    }
    expect((await listClients()).find(c => c.id === client.id)!.version).toBe(client.version);
  });

  it('applies the same rule to a group update', async () => {
    const { rmcpFixtureCall } = await import('./rmcpFixtures');
    const group = (await listGroups()).find(g => g.name === 'media')!;
    await expect(
      rmcpFixtureCall('rmcp_group_update', { id: group.id, patterns: ['media::*'] }),
    ).rejects.toMatchObject({ kind: 'invalid' });
    expect((await listGroups()).find(g => g.id === group.id)!.version).toBe(group.version);
  });

  it('still distinguishes a STALE version (conflict) from a missing one (invalid)', async () => {
    const client = (await listClients()).find(c => c.name === 'Reading assistant')!;
    await expect(
      updateClient({ id: client.id, version: client.version + 99, enabled: true }),
    ).rejects.toMatchObject({ kind: 'conflict' });
  });
});

describe('a bare * is operator-only, in the fixture as in production', () => {
  it('refuses it for this DELEGATED-owner principal, at write time', async () => {
    await expect(
      createGroup({ name: 'everything', description: '', patterns: ['*'] }),
    ).rejects.toMatchObject({ kind: 'invalid' });
  });

  it('reports it as rejected in the live preview, so the editor says so before saving', async () => {
    const preview = await previewGroup(['*']);
    expect(preview.invalidPatterns.map(p => p.pattern)).toEqual(['*']);
    expect(preview.invalidPatterns[0].reason).toMatch(/operator-only/);
    // And it matches nothing in the meantime — a refused pattern must not resolve.
    expect(preview.tools).toEqual([]);
  });

  it('pins BOTH sides of the ownership rule, so the fixture is not merely strict', async () => {
    const { patternRejection } = await import('./rmcpFixtures');
    // Delegated owner: refused.
    expect(patternRejection('*', false)).toMatch(/operator-only/);
    // Operator: permitted — the rule is conditional, not a blanket ban, and a fixture that
    // refused it for everyone would misstate it in the opposite direction.
    expect(patternRejection('*', true)).toBeNull();
    // Ownership changes nothing about the rest of the vocabulary.
    expect(patternRejection('media::*', false)).toBeNull();
    expect(patternRejection('ba*d', true)).toMatch(/wildcard may only appear at the end/);
  });
});

describe('a revoke must name EXACTLY ONE target — at both ends', () => {
  it('the client refuses an AMBIGUOUS revoke rather than picking one by precedence', async () => {
    // Structurally valid against the union (an object with both fields satisfies either member),
    // so the type system does not catch this — which is the point.
    await expect(
      revokeSessions({ sessionId: 's-1', clientRowId: 'c-1' } as unknown as { sessionId: string }),
    ).rejects.toMatchObject({ kind: 'invalid' });
  });

  it('the FIXTURE SERVER refuses an ambiguous revoke independently of the wrapper', async () => {
    const { rmcpFixtureCall } = await import('./rmcpFixtures');
    await expect(
      rmcpFixtureCall('rmcp_session_revoke', { session_id: 's-1', client_id: 'c-1' }),
    ).rejects.toMatchObject({ kind: 'invalid' });
  });

  it('an ambiguous revoke changes nothing — not even the selector it would have preferred', async () => {
    // The failure this guards against is a PARTIAL success reported as a success: the operator
    // asked for two revocations and got one.
    //
    // Targets s-3/c-3 deliberately. An earlier draft used s-1/c-1 and passed even against the
    // pre-fix code — the `sessions` describe above revokes c-1's sessions before this runs, so
    // "nothing changed" was true no matter what. A guard that cannot fail is not a guard; caught
    // by running the non-vacuity check on this test rather than only on its neighbours. s-3
    // belongs to c-3, which nothing else in this file touches.
    const { rmcpFixtureCall } = await import('./rmcpFixtures');
    const live = (await listSessions()).find(s => s.id === 's-3');
    expect(live?.revokedAt).toBeNull(); // precondition, asserted so it cannot rot silently

    await rmcpFixtureCall('rmcp_session_revoke', { session_id: 's-3', client_id: 'c-3' }).catch(() => undefined);

    const after = (await listSessions()).find(s => s.id === 's-3');
    expect(after?.revokedAt).toBeNull();
  });

  it('the client refuses a targetless revoke at runtime, not only in the type system', async () => {
    // Types are erased; this call is reachable from untyped JS. The cast is the point of the test.
    await expect(revokeSessions({} as unknown as { sessionId: string })).rejects.toMatchObject({
      kind: 'invalid',
    });
  });

  it('the FIXTURE SERVER refuses it independently of what the client would send', async () => {
    // Straight at the server boundary, bypassing the wrapper's guard entirely: a server may never
    // rely on its callers being well-behaved. Answering success here would tell an operator that
    // access was cut when nothing was touched — the failure that stops an investigation.
    const { rmcpFixtureCall } = await import('./rmcpFixtures');
    await expect(rmcpFixtureCall('rmcp_session_revoke', {})).rejects.toMatchObject({ kind: 'invalid' });
  });

  it('and it changes nothing when refused', async () => {
    const { rmcpFixtureCall } = await import('./rmcpFixtures');
    const before = await listSessions();
    await rmcpFixtureCall('rmcp_session_revoke', {}).catch(() => undefined);
    const after = await listSessions();
    expect(after.map(s => s.revokedAt)).toEqual(before.map(s => s.revokedAt));
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

// ── TERM-647: the create call carries the ownership the server refuses to guess ───────────────
//
// The bug this replaces was silent in exactly the way that keeps a bug alive: `createClient`
// omitted a REQUIRED argument, so every create failed at the server and the dialog showed a
// generic message. These tests pin both halves — that the wire arguments are actually sent, and
// that omitting them is refused rather than defaulted.
describe('creating a connector names its owner', () => {
  it('succeeds when both accounts are named and the pairing is allowed', async () => {
    const created = await createClient({
      owner: 'delegated-owner',
      actor: 'delegated-owner',
      name: 'Named owner',
      redirectUris: [],
      confidential: false,
      toolGroupIds: [],
      namespaces: [],
    });
    expect(created.client.name).toBe('Named owner');
    expect(created.clientSecret).toBeNull();
  });

  it('actually TRANSMITS both values — proven by the server distinguishing them', async () => {
    // The created record does not echo its owner back (`RmcpClient` has no owner field), so a
    // success assertion would pass just as happily with the fields dropped — which is precisely
    // the shape of the original bug. What proves transmission is the server telling the two
    // values APART: it can only refuse this pairing if it received both, and received them under
    // the argument names it declares.
    const swapped = await createClient({
      owner: 'studio-owner',
      actor: 'delegated-owner',
      name: 'Distinguishable',
      redirectUris: [],
      confidential: false,
      toolGroupIds: [],
      namespaces: [],
    }).catch(e => e);
    expect(swapped).toBeInstanceOf(RmcpError);
    expect(swapped.tool).toBe('rmcp_client_create');
    // ...and a value only the OWNER field could carry produces the owner-shaped refusal.
    const unknownOwner = await createClient({
      owner: 'nobody-by-that-name',
      actor: 'delegated-owner',
      name: 'Distinguishable',
      redirectUris: [],
      confidential: false,
      toolGroupIds: [],
      namespaces: [],
    }).catch(e => e);
    expect(unknownOwner.kind).toBe('not_found');
    expect(swapped.kind).not.toBe(unknownOwner.kind);
  });

  it('is REFUSED, not defaulted, when the owner is missing or blank', async () => {
    const { rmcpFixtureCall } = await import('./rmcpFixtures');
    // Straight at the server boundary: a caller that skips the wrapper (or a wrapper that
    // regresses to omitting the field) must still be refused. "Present but blank" is tested
    // alongside absent because a trimmed-to-empty string is the shape a form actually produces.
    await expect(rmcpFixtureCall('rmcp_client_create', { name: 'Ownerless' })).rejects.toMatchObject({
      kind: 'invalid',
    });
    await expect(
      rmcpFixtureCall('rmcp_client_create', { name: 'Blank', owner: '   ', actor: 'delegated-owner' }),
    ).rejects.toMatchObject({ kind: 'invalid' });
    await expect(
      rmcpFixtureCall('rmcp_client_create', { name: 'No actor', owner: 'delegated-owner' }),
    ).rejects.toMatchObject({ kind: 'invalid' });
  });

  it('refuses an unknown account without confirming which one is unknown', async () => {
    const err = await createClient({
      owner: 'nobody-by-that-name',
      actor: 'delegated-owner',
      name: 'Ghost',
      redirectUris: [],
      confidential: false,
      toolGroupIds: [],
      namespaces: [],
    }).catch(e => e);
    expect(err.kind).toBe('not_found');
    // A disabled account and a missing one collapse into this one answer on the real server, so
    // the fixture must not be more informative than the thing it stands in for.
    expect(err.message).not.toContain('nobody-by-that-name');
  });

  it('refuses to mint a connector owned by someone else — the check auto-filling would dissolve', async () => {
    const err = await createClient({
      owner: 'studio-owner',
      actor: 'delegated-owner',
      name: 'Not mine to give',
      redirectUris: [],
      confidential: false,
      toolGroupIds: [],
      namespaces: [],
    }).catch(e => e);
    // This is precisely the refusal a GUI defeats by copying `owner` into `actor`: the server
    // permits a create when actor IS owner, so an auto-filled actor satisfies it unconditionally.
    expect(err.kind).toBe('forbidden');
  });

  it('creates nothing when the ownership is refused', async () => {
    const before = (await listClients()).length;
    await createClient({
      owner: 'studio-owner',
      actor: 'delegated-owner',
      name: 'Should not exist',
      redirectUris: [],
      confidential: false,
      toolGroupIds: [],
      namespaces: [],
    }).catch(() => undefined);
    expect((await listClients()).length).toBe(before);
  });
});

// ── Accounts (TERM #654) ─────────────────────────────────────────────────────
//
// These run against the fixture server exactly as the connector tests do, and they exist because
// review round 2 pointed at a real hole in the round-1 test set: every added test covered a PURE
// helper, so a broken wire→client translation passed CI untouched. The defect it missed was not
// subtle in effect — the tool emits `created_at`, the client declares `createdAt`, the rows were
// spread rather than mapped, and `a.createdAt.slice(0, 10)` threw for every non-empty result.
// A page that worked only while it had nothing to show.
describe('accounts adapter', () => {
  it('TRANSLATES the wire shape rather than spreading it', async () => {
    const view = await listAccounts();
    expect(view.accounts.length).toBeGreaterThan(0);
    for (const a of view.accounts) {
      // The assertion that would have caught the shipped bug: a real RFC-3339 string, not
      // `undefined`, and reachable by the name the page actually reads.
      expect(typeof a.createdAt).toBe('string');
      expect(a.createdAt).toMatch(/^\d{4}-\d{2}-\d{2}/);
      expect(a.account).toBeTruthy();
      // No snake_case key survives the translation, and no credential field appears.
      expect(Object.keys(a).sort()).toEqual(['account', 'createdAt', 'disabled', 'id', 'operator']);
    }
  });

  it('reports the server flags rather than inferring them from the list length', async () => {
    // All THREE states, because round 3 caught the earlier version exercising only the healthy
    // one — where both flags are false, so it passed against a client that hard-coded them.
    const { __setFixtureAccounts } = await import('./rmcpFixtures');
    try {
      const healthy = await listAccounts();
      expect(healthy.accounts.length).toBeGreaterThan(0);
      expect(healthy.bootstrapAvailable).toBe(false);
      expect(healthy.stranded).toBe(false);

      // A door that has never had an account: the bootstrap is OPEN, and the empty list is not
      // what says so.
      __setFixtureAccounts('empty');
      const empty = await listAccounts();
      expect(empty.accounts).toEqual([]);
      expect(empty.bootstrapAvailable).toBe(true);
      expect(empty.stranded).toBe(false);

      // Accounts exist but none can administer: the list is EMPTY here too, and the flags are
      // the only thing distinguishing it from the case above. Inferring from length would get
      // this exactly backwards and offer a bootstrap that the server refuses.
      __setFixtureAccounts('stranded');
      const stranded = await listAccounts();
      expect(stranded.accounts).toEqual([]);
      expect(stranded.stranded).toBe(true);
      expect(stranded.bootstrapAvailable).toBe(false);
    } finally {
      __setFixtureAccounts('default');
    }
  });

  it('never carries a password in either direction', async () => {
    const created = await createAccount({
      account: 'adapter-test-account',
      password: 'a-long-enough-passphrase',
      operator: false,
    });
    expect(JSON.stringify(created)).not.toContain('a-long-enough-passphrase');
    expect(Object.keys(created).sort()).toEqual(['account', 'bootstrap', 'id', 'operator']);

    const view = await listAccounts();
    expect(JSON.stringify(view)).not.toContain('a-long-enough-passphrase');
    expect(JSON.stringify(view)).not.toContain('password');
  });

  it('surfaces the server refusal when the last active operator would be removed', async () => {
    // The fixture enforces the same guard the server does, so this exercises the path the page
    // takes when its disabled-button courtesy is bypassed — which is the whole reason that
    // courtesy is not the guard.
    await expect(setAccountOperator('primary', false)).rejects.toThrow(/last active operator/);
    await expect(setAccountDisabled('primary', true)).rejects.toThrow(/last active operator/);
  });

  it('requires an actor for a WRITE once several operators are active, but not for the READ', async () => {
    await setAccountOperator('delegate', true);
    // The read still works with no actor — otherwise the page could never render the picker it
    // needs in order to name one.
    const view = await listAccounts();
    expect(view.accounts.length).toBeGreaterThan(1);
    // The write refuses until one is named.
    await expect(setAccountOperator('delegate', false)).rejects.toThrow(/more than one operator/);
    await expect(setAccountOperator('delegate', false, 'primary')).resolves.toBeUndefined();
  });
});
