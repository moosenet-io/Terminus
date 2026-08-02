// MUSE-128 review round 2: proves `MuseSection.refetch` ACTUALLY resolves a `Promise<boolean>`
// in production, not just in a test double. The prior round's `useActivityFeedLive` backoff
// test faked a promise-returning `refetch` that `useMuseSection`'s real `fetchOnce` never
// supplied (it returned `void`) — this file is the missing other half: proof the REAL hook now
// honours the contract `useActivityFeedLive.ts` is written against, for both underlying
// generators (`useMuseSection`, backing `useMuseStats` et al., and `useMuseTypedSection`,
// backing `useMuseLiveSessions`/`useMuseSessionHistory`).
//
// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import { useMuseStats, useMuseLiveSessions } from './useMuse';

const requestMock = vi.fn();
const liveMock = vi.fn();

vi.mock('../lib/aggregationClient', () => ({
  getAggregationClient: () => ({
    request: requestMock,
    muse: { sessions: { live: liveMock } },
  }),
}));

describe('useMuseSection-backed hooks (useMuseStats) — refetch resolves Promise<boolean>', () => {
  it('resolves true on a successful fetch', async () => {
    requestMock.mockResolvedValueOnce({ library_size: 10, pending_items: 0, last_ingest_at: null });
    const { result } = renderHook(() => useMuseStats());
    await waitFor(() => expect(result.current.loading).toBe(false));

    requestMock.mockResolvedValueOnce({ library_size: 11, pending_items: 0, last_ingest_at: null });
    await expect(result.current.refetch()).resolves.toBe(true);
  });

  it('resolves false (never rejects) on an errored fetch', async () => {
    requestMock.mockResolvedValueOnce({ library_size: 10, pending_items: 0, last_ingest_at: null });
    const { result } = renderHook(() => useMuseStats());
    await waitFor(() => expect(result.current.loading).toBe(false));

    requestMock.mockRejectedValueOnce(new Error('HTTP 500 for /stats'));
    await expect(result.current.refetch()).resolves.toBe(false);
  });

  it('resolves false (never rejects) on the mock-not-wired sentinel (null/undefined)', async () => {
    requestMock.mockResolvedValueOnce({ library_size: 10, pending_items: 0, last_ingest_at: null });
    const { result } = renderHook(() => useMuseStats());
    await waitFor(() => expect(result.current.loading).toBe(false));

    requestMock.mockResolvedValueOnce(null);
    await expect(result.current.refetch()).resolves.toBe(false);
  });
});

describe('useMuseTypedSection-backed hooks (useMuseLiveSessions) — refetch resolves Promise<boolean>', () => {
  it('resolves true when the envelope reports available:true', async () => {
    liveMock.mockResolvedValue({ available: true, sessions: [], source: 'muse-derived' });
    const { result } = renderHook(() => useMuseLiveSessions());
    await waitFor(() => expect(result.current.loading).toBe(false));

    await expect(result.current.refetch()).resolves.toBe(true);
  });

  it('resolves false when the envelope reports available:false', async () => {
    liveMock.mockResolvedValueOnce({ available: true, sessions: [], source: 'muse-derived' });
    const { result } = renderHook(() => useMuseLiveSessions());
    await waitFor(() => expect(result.current.loading).toBe(false));

    liveMock.mockResolvedValueOnce({ available: false, sessions: [], source: 'muse-derived', detail: 'not yet wired' });
    await expect(result.current.refetch()).resolves.toBe(false);
  });
});
