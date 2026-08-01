// MACT-03 (MUSE #123): coverage for the Maestro Activity typed session surfaces --
// `muse.sessions.live()` / `.history()` / `.terminate()` on both adapters, the grep-enforced
// "only aggregationClient.ts calls fetch/WebSocket" rule, and the LiveSession/HistorySession
// type-level distinctness this item's whole design turns on.
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { mockAdapter, httpAdapter } from './aggregationClient';
import type { LiveSession, HistorySession } from './aggregationClient';

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

describe('MACT-03: aggregationClient.ts is the only module calling fetch/WebSocket', () => {
  it('no other .ts/.tsx source file under src/ calls fetch( or constructs new WebSocket(', () => {
    const offenders: string[] = [];
    for (const [path, text] of Object.entries(RAW_SOURCES)) {
      if (ALLOWED_FILES.has(path) || path.endsWith('.test.ts') || path.endsWith('.test.tsx')) continue;
      // A bare `fetch(` call or a `new WebSocket(` construction -- not merely the word "fetch"
      // appearing in prose/identifiers like `fetchOnce`/`refetch`.
      if (/(?<![.\w])fetch\s*\(/.test(text) || /new\s+WebSocket\s*\(/.test(text)) {
        offenders.push(path);
      }
    }
    expect(offenders, `files outside aggregationClient.ts calling fetch/WebSocket: ${offenders.join(', ')}`)
      .toEqual([]);
  });
});

// ── Type-level distinctness: LiveSession and HistorySession are NOT interchangeable ───────────

describe('MACT-03: LiveSession and HistorySession are distinct types', () => {
  it('a HistorySession is not assignable where a LiveSession is required (missing state/last_event_at)', () => {
    const history: HistorySession = {
      session_id: 1, session_key: 'k', account: { id: 1, display_name: 'x' },
      item: { media_item_id: 1, title: 't', year: 2020, kind: 'movie', season_number: null, episode_number: null, episode_title: null },
      poster_url: null, backdrop_url: null, view_offset_ms: 0, duration_ms: 1000, progress_pct: 0,
      player: null, platform: null, product: null, device: null, started_at: '2026-01-01T00:00:00Z',
      decision: { video_decision: null, audio_decision: null, transcode_decision: null, transcode_reason: null, container: null, video_codec: null, audio_codec: null, audio_channels: null, video_resolution: null, bitrate: null },
    };
    // @ts-expect-error -- HistorySession lacks `state`/`last_event_at`; assigning it where a
    // LiveSession is expected must be a compile error, proving the two types are not merged.
    const asLive: LiveSession = history;
    expect(asLive).toBeDefined();
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

  it('terminate() success reports the real outcome fields, never fabricating success', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ stopped: false, backend: 'plex', reason_delivered: false }), { status: 200 }),
    );
    const res = await httpAdapter.muse.sessions.terminate('x', 'operator requested stop');
    expect(res).toEqual({ kind: 'ok', stopped: false, backend: 'plex', reason_delivered: false });
  });
});
