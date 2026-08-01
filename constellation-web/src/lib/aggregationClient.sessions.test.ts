// MACT-03 (MUSE #123): coverage for the Maestro Activity typed session surfaces --
// `muse.sessions.live()` / `.history()` / `.terminate()` on both adapters, the grep-enforced
// "only aggregationClient.ts calls fetch/WebSocket" rule, and the LiveSession/HistorySession
// type-level distinctness this item's whole design turns on.
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { mockAdapter, httpAdapter, onMutationResult } from './aggregationClient';
import type { LiveSession, HistorySession, MutationResultEvent } from './aggregationClient';

// ── Grep assertion: aggregationClient.ts is the ONLY module calling fetch/WebSocket ──────────
//
// This is the acceptance-criterion check the module doc comment promises ("grep-enforced").
// Reads every .ts/.tsx source file under src/ (via Vite's `import.meta.glob` -- this project has
// no `@types/node`, so this stays off `node:fs` deliberately, same reasoning as the rest of the
// browser-only client code) except this file and aggregationClient.ts itself, and fails if any
// of them call `fetch(` or construct `new WebSocket(`.
// `import.meta.glob` is a Vite build-time macro that only fires on the LITERAL
// `import.meta.glob(...)` call syntax (Vite statically rewrites it -- an indirection through a
// destructured reference does not trigger the transform). This project has no `vite/client`
// types pulled in (see `resolveMode()`'s own `import.meta as unknown as {...}` cast in
// aggregationClient.ts for the same "cast around missing ambient types" pattern), hence the
// `@ts-expect-error` rather than a typed wrapper.
// @ts-expect-error -- import.meta.glob has no ambient type in this project (see comment above)
const RAW_SOURCES: Record<string, string> = import.meta.glob('/src/**/*.{ts,tsx}', { query: '?raw', import: 'default', eager: true });
const ALLOWED_FILES = new Set(['/src/lib/aggregationClient.ts']);

// A call whose callee identifier is exactly `fetch`, optionally reached via a `window.`/
// `globalThis.`/`self.` member prefix (all three are the same global in a browser) -- NOT merely
// the substring "fetch" appearing inside a longer identifier. The leading `(?:^|[^.\w])` boundary
// is what excludes `prefetch(`, `refetch(`, `doFetch(` (a word character immediately precedes
// "fetch" there) while still allowing `window.fetch(` (the character before "window" is the
// boundary, not the character before "fetch"). A prior version of this regex excluded ANY `fetch`
// preceded by `.`, which wrongly passed `window.fetch(`/`globalThis.fetch(`/`self.fetch(` --
// fixed here; see the self-test below that pins the member-form case.
//
// KNOWN LIMITATION (not solved by this regex, and not solved by any static text scan): an alias
// assignment (`const request = fetch; request(...)`) evades this check entirely, same as it would
// evade a real ESLint `no-restricted-globals` rule without additional scope analysis. This is a
// best-effort textual guard, not a sandboxed guarantee -- code review is still the backstop for a
// deliberately obfuscated bypass.
const FETCH_CALL = /(?:^|[^.\w])(?:(?:window|globalThis|self)\s*\.\s*)?fetch\s*\(/;
const WEBSOCKET_CTOR = /new\s+(?:(?:window|globalThis|self)\s*\.\s*)?WebSocket\s*\(/;

function callsFetchOrConstructsWebSocket(text: string): boolean {
  return FETCH_CALL.test(text) || WEBSOCKET_CTOR.test(text);
}

describe('MACT-03: the fetch/WebSocket-exclusivity guard actually fires (self-test)', () => {
  it('detects a bare fetch( call', () => {
    expect(callsFetchOrConstructsWebSocket('async function f() { return fetch("/x"); }')).toBe(true);
  });

  it('detects window.fetch( / globalThis.fetch( / self.fetch( -- the regression this fix closes', () => {
    expect(callsFetchOrConstructsWebSocket('window.fetch("/x")')).toBe(true);
    expect(callsFetchOrConstructsWebSocket('globalThis.fetch("/x")')).toBe(true);
    expect(callsFetchOrConstructsWebSocket('self.fetch("/x")')).toBe(true);
  });

  it('detects new WebSocket( and new window.WebSocket(', () => {
    expect(callsFetchOrConstructsWebSocket('const ws = new WebSocket(url);')).toBe(true);
    expect(callsFetchOrConstructsWebSocket('const ws = new window.WebSocket(url);')).toBe(true);
  });

  it('does NOT flag identifiers that merely end in "fetch" (prefetch/refetch/doFetch/fetchOnce)', () => {
    expect(callsFetchOrConstructsWebSocket('prefetch(url);')).toBe(false);
    expect(callsFetchOrConstructsWebSocket('refetch();')).toBe(false);
    expect(callsFetchOrConstructsWebSocket('doFetch(url);')).toBe(false);
    expect(callsFetchOrConstructsWebSocket('const fetchOnce = useCallback(() => {}, []);')).toBe(false);
    expect(callsFetchOrConstructsWebSocket('return { refetch: fetchOnce };')).toBe(false);
  });
});

describe('MACT-03: aggregationClient.ts is the only module calling fetch/WebSocket', () => {
  it('no other .ts/.tsx source file under src/ calls fetch( or constructs new WebSocket(', () => {
    const offenders: string[] = [];
    for (const [path, text] of Object.entries(RAW_SOURCES)) {
      if (ALLOWED_FILES.has(path) || path.endsWith('.test.ts') || path.endsWith('.test.tsx')) continue;
      if (callsFetchOrConstructsWebSocket(text)) {
        offenders.push(path);
      }
    }
    expect(offenders, `files outside aggregationClient.ts calling fetch/WebSocket: ${offenders.join(', ')}`)
      .toEqual([]);
  });
});

// ── Type-level distinctness: LiveSession and HistorySession are NOT interchangeable ───────────

describe('MACT-03: LiveSession and HistorySession are distinct types', () => {
  const SHARED_DECISION = {
    video_decision: null, audio_decision: null, transcode_decision: null, transcode_reason: null,
    container: null, video_codec: null, audio_codec: null, audio_channels: null,
    video_resolution: null, bitrate: null,
  };
  const SHARED_FIELDS = {
    session_id: 1, session_key: 'k', account: { id: 1, display_name: 'x' },
    item: { media_item_id: 1, title: 't', year: 2020, kind: 'movie' as const, season_number: null, episode_number: null, episode_title: null },
    poster_url: null, backdrop_url: null, view_offset_ms: 0, duration_ms: 1000, progress_pct: 0,
    player: null, platform: null, product: null, device: null, started_at: '2026-01-01T00:00:00Z',
    decision: SHARED_DECISION,
  };

  it('a HistorySession is not assignable where a LiveSession is required (missing state/last_event_at/source)', () => {
    const history: HistorySession = { ...SHARED_FIELDS, source: 'muse-history' };
    // @ts-expect-error -- HistorySession lacks `state`/`last_event_at` AND carries the WRONG
    // `source` literal ('muse-history', not 'muse-derived'); assigning it where a LiveSession is
    // expected must be a compile error, proving the two types are not merged.
    const asLive: LiveSession = history;
    expect(asLive).toBeDefined();
  });

  it('a LiveSession is NOT assignable where a HistorySession is required, even though it is a structural SUPERSET (regression: this direction silently compiled before the `source` discriminant was added)', () => {
    const live: LiveSession = {
      ...SHARED_FIELDS, source: 'muse-derived', state: 'playing', last_event_at: null,
    };
    // @ts-expect-error -- LiveSession has every HistorySession field PLUS state/last_event_at,
    // so TypeScript's structural typing would otherwise accept it here silently (excess
    // properties are only checked on object LITERALS, not variables) -- exactly the drift this
    // item exists to forbid, and exactly what `source`'s mismatched literal type now blocks.
    const asHistory: HistorySession = live;
    expect(asHistory).toBeDefined();
  });

  it('runtime: a live session carries state/last_event_at; a history session does not', async () => {
    const live = await mockAdapter.muse.sessions.live();
    const hist = await mockAdapter.muse.sessions.history();
    expect(live.source).toBe('muse-derived');
    expect(hist.source).toBe('muse-history');
    for (const s of live.sessions) {
      expect('state' in s).toBe(true);
    }
    for (const s of hist.sessions as unknown as Record<string, unknown>[]) {
      expect('state' in s).toBe(false);
      expect('last_event_at' in s).toBe(false);
    }
  });
});

// ── progress_pct: absent (not null) when duration is unknown ─────────────────────────────────

describe('MACT-03: progress_pct is ABSENT (not null) when duration is unknown', () => {
  it('the mock live fixture covers a stale session with an unknown duration and no progress_pct key', async () => {
    const live = await mockAdapter.muse.sessions.live();
    const stale = live.sessions.find(s => s.state === 'stale');
    expect(stale, 'fixture must cover a stale session').toBeDefined();
    expect(stale!.duration_ms).toBeNull();
    expect('progress_pct' in stale!).toBe(false);
  });

  it('the mock live fixture covers playing and paused states too', async () => {
    const live = await mockAdapter.muse.sessions.live();
    const states = new Set(live.sessions.map(s => s.state));
    expect(states.has('playing')).toBe(true);
    expect(states.has('paused')).toBe(true);
    expect(states.has('stale')).toBe(true);
  });

  it('the mock fixtures cover direct-play, remux (direct_stream), and full transcode rows', async () => {
    const live = await mockAdapter.muse.sessions.live();
    const hist = await mockAdapter.muse.sessions.history();
    const allDecisions = [...live.sessions, ...hist.sessions].map(s => s.decision.video_decision);
    expect(allDecisions).toContain('direct_play');
    expect(allDecisions).toContain('transcode');
    const audioDecisions = [...live.sessions, ...hist.sessions].map(s => s.decision.audio_decision);
    expect(audioDecisions).toContain('direct_stream'); // the "remux" case
  });
});

// ── mock terminate ─────────────────────────────────────────────────────────────────────────

describe('MACT-03: mockAdapter.muse.sessions.terminate()', () => {
  it('a known live session_key resolves kind: ok', async () => {
    const live = await mockAdapter.muse.sessions.live();
    const key = live.sessions[0].session_key!;
    const res = await mockAdapter.muse.sessions.terminate(key);
    expect(res.kind).toBe('ok');
  });

  it('an unknown session_key resolves kind: not_found, never throws', async () => {
    const res = await mockAdapter.muse.sessions.terminate('does-not-exist');
    expect(res.kind).toBe('not_found');
  });

  it('the reserved ambiguous-session sentinel resolves kind: conflict, never throws', async () => {
    const res = await mockAdapter.muse.sessions.terminate('sess-mock-ambiguous');
    expect(res.kind).toBe('conflict');
  });
});

// ── httpAdapter: degrade-not-throw + typed 403 vs transport failure ──────────────────────────
//
// httpAdapter reads `window.location.origin` (the one sanctioned window read in this file) --
// there is no jsdom dependency in this project, so these tests stub a minimal `window` global
// directly rather than pull in a DOM environment for one file.

describe('MACT-03: httpAdapter.muse.sessions -- degrade-not-throw + typed terminate outcomes', () => {
  const originalWindow = (globalThis as { window?: unknown }).window;
  const originalFetch = globalThis.fetch;

  beforeEach(() => {
    (globalThis as unknown as { window: unknown }).window = { location: { origin: 'http://localhost:5174' } };
  });

  afterEach(() => {
    (globalThis as unknown as { window: unknown }).window = originalWindow;
    globalThis.fetch = originalFetch;
    vi.restoreAllMocks();
  });

  it('live() yields {available:false} on a 401 (unprovisioned bearer, TERM ticket 549) -- never throws', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(new Response(null, { status: 401 }));
    const res = await httpAdapter.muse.sessions.live();
    expect(res.available).toBe(false);
    expect(res.detail).toMatch(/401/);
    expect(res.sessions).toEqual([]);
  });

  it('history() yields {available:false} on a 404 (route not deployed) -- never throws', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(new Response(null, { status: 404 }));
    const res = await httpAdapter.muse.sessions.history();
    expect(res.available).toBe(false);
    expect(res.detail).toMatch(/404/);
  });

  it('live() yields {available:false} on a network failure -- never throws', async () => {
    globalThis.fetch = vi.fn().mockRejectedValue(new TypeError('network error'));
    const res = await httpAdapter.muse.sessions.live();
    expect(res.available).toBe(false);
  });

  it('terminate() 403 resolves a typed forbidden result, distinct from a network error', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ error: 'forbidden', required_role: 'operator' }), { status: 403 }),
    );
    const res = await httpAdapter.muse.sessions.terminate('sess-1');
    expect(res.kind).toBe('forbidden');
  });

  it('terminate() network failure resolves kind: error -- NOT forbidden', async () => {
    globalThis.fetch = vi.fn().mockRejectedValue(new TypeError('network error'));
    const res = await httpAdapter.muse.sessions.terminate('sess-1');
    expect(res.kind).toBe('error');
  });

  it('terminate() 404 resolves kind: not_found; 503 resolves kind: unavailable', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(new Response(null, { status: 404 }));
    expect((await httpAdapter.muse.sessions.terminate('x')).kind).toBe('not_found');

    globalThis.fetch = vi.fn().mockResolvedValue(new Response(null, { status: 503 }));
    expect((await httpAdapter.muse.sessions.terminate('x')).kind).toBe('unavailable');
  });

  it('terminate() 409 resolves kind: conflict, distinct from not_found and error (MACT-02 AmbiguousSession/AmbiguousTarget)', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      new Response(
        JSON.stringify({ error: 'more than one live session currently matches session_key x; refusing to guess which one to stop' }),
        { status: 409 },
      ),
    );
    const res = await httpAdapter.muse.sessions.terminate('x');
    expect(res.kind).toBe('conflict');
    expect(res.kind).not.toBe('not_found');
    expect(res.kind).not.toBe('error');
    if (res.kind === 'conflict') {
      expect(res.detail).toMatch(/refusing to guess/);
    }
  });

  it("terminate() reads the real Muse error body ({\"error\": ...}) as the typed result's detail", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ error: 'no live session for session_key x' }), { status: 404 }),
    );
    const res = await httpAdapter.muse.sessions.terminate('x');
    expect(res).toMatchObject({ kind: 'not_found', detail: 'no live session for session_key x' });
  });

  it('terminate() success reports the real outcome fields, never fabricating success', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ stopped: false, backend: 'plex', reason_delivered: false }), { status: 200 }),
    );
    const res = await httpAdapter.muse.sessions.terminate('x', 'operator requested stop');
    expect(res).toEqual({ kind: 'ok', stopped: false, backend: 'plex', reason_delivered: false });
  });

  // Review finding: `TerminateSessionResponse`'s three fields are all REQUIRED in Rust (no
  // `Option`, no `skip_serializing_if`) -- a 2xx with a missing/malformed field must never be
  // upgraded into an invented 'ok' outcome (that would claim something the response never
  // established, the same class of bug MACT-02's own review caught server-side).
  it('terminate() a 204 (no body) on 2xx resolves kind: error, never a fabricated ok', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(new Response(null, { status: 204 }));
    const res = await httpAdapter.muse.sessions.terminate('x');
    expect(res.kind).toBe('error');
  });

  it('terminate() an empty {} body on 200 resolves kind: error, never {stopped:false,backend:"unknown",...}', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify({}), { status: 200 }));
    const res = await httpAdapter.muse.sessions.terminate('x');
    expect(res.kind).toBe('error');
  });

  it('terminate() a 200 with a wrong-typed field (stopped as a string) resolves kind: error', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ stopped: 'yes', backend: 'plex', reason_delivered: false }), { status: 200 }),
    );
    const res = await httpAdapter.muse.sessions.terminate('x');
    expect(res.kind).toBe('error');
  });

  // Review finding: a non-string `error` field (a misbehaving proxy/backend sending
  // `{"error": 42}`) must not leak a non-string value into `detail`, which the type declares as
  // `string`.
  it('terminate() a non-string error field falls back to a generic string detail, never a raw number', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ error: 42 }), { status: 404 }),
    );
    const res = await httpAdapter.muse.sessions.terminate('x');
    expect(res.kind).toBe('not_found');
    if (res.kind === 'not_found') {
      expect(typeof res.detail).toBe('string');
      expect(res.detail).toBe('session not in the live set');
    }
  });
});

// ── Mutation-result activity event reflects the REAL outcome, not just resolution ────────────
//
// Review finding: `terminate()` never throws (it resolves a discriminated union), but
// `withMutationResultEvent` used to infer `ok: true` from bare resolution -- so a refused
// termination (403/404/409/503) emitted a SUCCESS activity event/toast, the same "reports
// something stronger than what happened" defect MACT-02 was corrected for server-side. These
// assert on the EMITTED EVENT itself, not just the returned union -- that gap is what let the
// defect through undetected in the prior round.

/** Subscribes to exactly the next `MutationResultEvent`, resolving once one fires. */
function nextMutationResult(): Promise<MutationResultEvent> {
  return new Promise(resolve => {
    const unsubscribe = onMutationResult(event => {
      unsubscribe();
      resolve(event);
    });
  });
}

describe('MACT-03: mutation-result activity event classifies terminate() outcomes correctly', () => {
  const originalWindow = (globalThis as { window?: unknown }).window;
  const originalFetch = globalThis.fetch;

  beforeEach(() => {
    (globalThis as unknown as { window: unknown }).window = { location: { origin: 'http://localhost:5174' } };
  });

  afterEach(() => {
    (globalThis as unknown as { window: unknown }).window = originalWindow;
    globalThis.fetch = originalFetch;
    vi.restoreAllMocks();
  });

  it('httpAdapter: a 403 (forbidden) result emits ok:false, not a false success toast', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ error: 'forbidden' }), { status: 403 }),
    );
    const eventPromise = nextMutationResult();
    const res = await httpAdapter.muse.sessions.terminate('sess-1');
    const event = await eventPromise;
    expect(res.kind).toBe('forbidden');
    expect(event.ok).toBe(false);
    expect(event.error).toBeTruthy();
  });

  it('httpAdapter: a 409 (conflict) result emits ok:false', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ error: 'more than one live session matches; refusing to guess' }), { status: 409 }),
    );
    const eventPromise = nextMutationResult();
    const res = await httpAdapter.muse.sessions.terminate('sess-1');
    const event = await eventPromise;
    expect(res.kind).toBe('conflict');
    expect(event.ok).toBe(false);
    expect(event.error).toMatch(/refusing to guess/);
  });

  it('httpAdapter: a genuine ok result emits ok:true', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ stopped: true, backend: 'plex', reason_delivered: false }), { status: 200 }),
    );
    const eventPromise = nextMutationResult();
    const res = await httpAdapter.muse.sessions.terminate('sess-1');
    const event = await eventPromise;
    expect(res.kind).toBe('ok');
    expect(event.ok).toBe(true);
  });

  it('mockAdapter: the reserved ambiguous-session sentinel (conflict) emits ok:false', async () => {
    const eventPromise = nextMutationResult();
    const res = await mockAdapter.muse.sessions.terminate('sess-mock-ambiguous');
    const event = await eventPromise;
    expect(res.kind).toBe('conflict');
    expect(event.ok).toBe(false);
  });

  it('mockAdapter: an unknown session_key (not_found) emits ok:false', async () => {
    const eventPromise = nextMutationResult();
    const res = await mockAdapter.muse.sessions.terminate('does-not-exist');
    const event = await eventPromise;
    expect(res.kind).toBe('not_found');
    expect(event.ok).toBe(false);
  });

  it('mockAdapter: a known live session (ok) emits ok:true', async () => {
    const live = await mockAdapter.muse.sessions.live();
    const key = live.sessions[0].session_key!;
    const eventPromise = nextMutationResult();
    const res = await mockAdapter.muse.sessions.terminate(key);
    const event = await eventPromise;
    expect(res.kind).toBe('ok');
    expect(event.ok).toBe(true);
  });
});
