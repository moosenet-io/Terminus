// WIRE-05, ported for CONST-04: Chord API fetch helpers. Routed through the aggregation
// client (system 'chord') instead of a direct `/chord` fetch — no panel currently uses this,
// kept as the generic escape hatch for future Chord panels that need ad hoc reads/writes.
import { getAggregationClient } from '../lib/aggregationClient';

export function useChordFetch() {
  async function chordGet<T>(path: string): Promise<T | null> {
    try {
      return await getAggregationClient().request<T>('chord', path);
    } catch { return null; }
  }

  async function chordPost<T>(path: string, body: unknown): Promise<T | null> {
    try {
      return await getAggregationClient().request<T>('chord', path, {
        method: 'POST',
        body: JSON.stringify(body),
      });
    } catch { return null; }
  }

  return { chordGet, chordPost };
}
