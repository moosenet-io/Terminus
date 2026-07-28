// LGUI-09 (§3.4): data + mutation hook for the Persona & Behavior panel. Owns the ONE state
// source `PersonaPanel.tsx`'s TraitSlider quartet and radar thumbnail both read — the spec's
// explicit AC "radar + sliders never disagree" is only true because both render straight off
// `traits` here; neither component keeps its own copy. `saveTraits`/`saveContext` mutate via
// the generic `client.request('lumina', ...)` escape hatch (same convention as `useMuse.ts`'s
// `useMuseChannelActions` — no typed `AggregationClient` method needed for a single panel's
// mutations) and, on success, adopt the SERVER's re-clamped response as the new state (never
// trust the client-side optimistic value as the source of truth for what got saved).
import { useCallback, useEffect, useState } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import {
  LUMINA_TRAIT_KEYS,
  PERSONA_DEFAULT_BOUNDS,
} from '../types/lumina';
import type {
  LuminaPersonaResponse,
  LuminaPersonaBounds,
  LuminaPersonaStatusFlags,
  LuminaPromptLayer,
  LuminaTraitVector,
  LuminaPersonaTraitsWriteBody,
  LuminaPersonaTraitsWriteResponse,
  LuminaPersonaContextWriteResponse,
} from '../types/lumina';

/** Mirrors the server's `effective = clamp(base + modifier)` (§0.1.1,
 *  `prompt/multi_personality.rs`) so a slider preview never shows an out-of-bounds value even
 *  before the save round-trip returns the server's own clamp. */
export function clampToPersonaBounds(v: number, bounds: LuminaPersonaBounds = PERSONA_DEFAULT_BOUNDS): number {
  return Math.min(bounds.max, Math.max(bounds.min, v));
}

export function computeEffective(
  base: LuminaTraitVector,
  modifier: LuminaTraitVector,
  bounds: LuminaPersonaBounds = PERSONA_DEFAULT_BOUNDS,
): LuminaTraitVector {
  const out = {} as LuminaTraitVector;
  for (const k of LUMINA_TRAIT_KEYS) {
    out[k] = clampToPersonaBounds(base[k] + modifier[k], bounds);
  }
  return out;
}

interface PersonaState {
  data: LuminaPersonaResponse | null;
  loading: boolean;
  isRefetching: boolean;
  error: string | null;
}

interface StatusState {
  data: LuminaPersonaStatusFlags | null;
  loading: boolean;
  error: string | null;
}

export interface UseLuminaPersonaResult {
  /** `null` only while the very first load is in flight or it failed outright — every render
   *  after that always has SOME traits/bounds/layers to show (possibly stale, via
   *  `isRefetching`), which is what lets the radar and sliders share one source without a
   *  window where one has data and the other doesn't. */
  persona: LuminaPersonaResponse | null;
  loading: boolean;
  isRefetching: boolean;
  error: string | null;
  refetch: () => void;
  status: LuminaPersonaStatusFlags | null;
  statusLoading: boolean;
  /** Saves a traits diff (whichever of base/modifier changed — admin edits base, per-user
   *  modifier is admin-on-behalf v1, §3.4) and adopts the server's re-clamped response. Throws
   *  on failure — callers (the ConfirmDialog's onConfirm) are expected to catch and surface it,
   *  same pattern as every other mutating call in this app. */
  saveTraits: (body: LuminaPersonaTraitsWriteBody) => Promise<LuminaPersonaTraitsWriteResponse>;
  saveContext: (activeContext: string) => Promise<LuminaPersonaContextWriteResponse>;
}

const POLL_MS = 30_000;

export function useLuminaPersona(user?: string): UseLuminaPersonaResult {
  const [persona, setPersona] = useState<PersonaState>({
    data: null, loading: true, isRefetching: false, error: null,
  });
  const [status, setStatus] = useState<StatusState>({ data: null, loading: true, error: null });

  const fetchPersona = useCallback((isRefetch: boolean) => {
    setPersona(s => ({ ...s, isRefetching: isRefetch }));
    const qs = user ? `?user=${encodeURIComponent(user)}` : '';
    getAggregationClient()
      .request<LuminaPersonaResponse | null>('lumina', `/persona${qs}`)
      .then(data => {
        if (data == null) {
          // mockAdapter's / a real 404's "not mocked/not yet wired" sentinel (see useMuse.ts's
          // identical convention) — surfaced as an error state, not a silent blank panel.
          setPersona({ data: null, loading: false, isRefetching: false, error: 'Persona API not available' });
          return;
        }
        setPersona({ data, loading: false, isRefetching: false, error: null });
      })
      .catch(e => setPersona(s => ({
        ...s, loading: false, isRefetching: false,
        error: e instanceof Error ? e.message : String(e),
      })));
  }, [user]);

  const fetchStatus = useCallback(() => {
    getAggregationClient()
      .request<LuminaPersonaStatusFlags | null>('lumina', '/status')
      .then(data => setStatus({ data: data ?? null, loading: false, error: null }))
      .catch(e => setStatus({ data: null, loading: false, error: e instanceof Error ? e.message : String(e) }));
  }, []);

  useEffect(() => { fetchPersona(false); }, [fetchPersona]);
  useEffect(() => { fetchStatus(); }, [fetchStatus]);

  useEffect(() => {
    const id = setInterval(() => fetchPersona(true), POLL_MS);
    return () => clearInterval(id);
  }, [fetchPersona]);

  const saveTraits = useCallback(async (body: LuminaPersonaTraitsWriteBody) => {
    const qs = user ? `?user=${encodeURIComponent(user)}` : '';
    const result = await getAggregationClient().request<LuminaPersonaTraitsWriteResponse>(
      'lumina',
      `/persona/traits${qs}`,
      { method: 'PUT', body: JSON.stringify(body) },
    );
    setPersona(s => (s.data ? { ...s, data: { ...s.data, traits: result } } : s));
    return result;
  }, [user]);

  const saveContext = useCallback(async (activeContext: string) => {
    const result = await getAggregationClient().request<LuminaPersonaContextWriteResponse>(
      'lumina',
      '/persona/context',
      { method: 'PUT', body: JSON.stringify({ active_context: activeContext }) },
    );
    setPersona(s => (s.data ? { ...s, data: { ...s.data, active_context: result.active_context } } : s));
    return result;
  }, []);

  return {
    persona: persona.data,
    loading: persona.loading,
    isRefetching: persona.isRefetching,
    error: persona.error,
    refetch: () => fetchPersona(false),
    status: status.data,
    statusLoading: status.loading,
    saveTraits,
    saveContext,
  };
}

/** Shapes the 11 `PromptAssembler` layers (§0.1.1) into the byte-bar rows the Layer Inspector
 *  renders — pure, no hook dependency, so it's independently testable if a test runner is ever
 *  wired up for this package (`npm run test` currently only covers `fleetRingBuffer.test.ts`). */
export function layerBarWidths(layers: LuminaPromptLayer[]): Array<LuminaPromptLayer & { pct: number }> {
  const maxBytes = Math.max(1, ...layers.map(l => l.bytes));
  return layers.map(l => ({ ...l, pct: Math.round((l.bytes / maxBytes) * 100) }));
}
