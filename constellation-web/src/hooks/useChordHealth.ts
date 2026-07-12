// WIRE-05, ported for CONST-04: Poll Chord health, models, and storage endpoints.
// Routed through the aggregation client (system 'chord') instead of direct `/chord/api` fetch.
import { useState, useEffect } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import type { ChordInferenceState, ChordModelRecord, ChordStorageLocation } from '../types/chord';

export function useChordHealth() {
  const [health, setHealth] = useState<ChordInferenceState | null>(null);
  const [models, setModels] = useState<ChordModelRecord[]>([]);
  const [storage, setStorage] = useState<ChordStorageLocation[]>([]);
  const [loading, setLoading] = useState(true);
  const [offline, setOffline] = useState(false);

  useEffect(() => {
    let cancelled = false;

    async function poll() {
      try {
        const client = getAggregationClient();
        const [h, m, s] = await Promise.all([
          client.request<ChordInferenceState | null>('chord', '/health').catch(() => null),
          client.request<ChordModelRecord[]>('chord', '/models').catch(() => []),
          client.request<ChordStorageLocation[] | { locations?: ChordStorageLocation[] }>('chord', '/storage')
            .then(d => Array.isArray(d) ? d : (d.locations ?? []))
            .catch(() => []),
        ]);
        if (!cancelled) {
          setHealth(h);
          setModels(Array.isArray(m) ? m : []);
          setStorage(Array.isArray(s) ? s : []);
          setOffline(!h);
          setLoading(false);
        }
      } catch {
        if (!cancelled) { setOffline(true); setLoading(false); }
      }
    }

    void poll();
    const interval = setInterval(() => void poll(), 3000);
    return () => { cancelled = true; clearInterval(interval); };
  }, []);

  return { health, models, storage, loading, offline };
}
