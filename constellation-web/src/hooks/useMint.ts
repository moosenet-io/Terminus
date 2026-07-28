// CGUI-10 (TERM #533): MINT module data hooks. Every MINT read goes through the CGUI-08 typed
// data client (`getAggregationClient().mint.*` / `.models.*`) — never a raw fetch — and every
// section fetches independently so one dead/empty endpoint degrades ONLY its own card (the same
// per-endpoint degrade boundary useMuse established). Degradation semantics:
//   - HTTP 404/501            → "not yet wired" (endpoint not live on this backend)
//   - a `mint.category*` 400  → treated as a hard error detail (unknown category — a bug, not
//                               an un-profiled one; an un-profiled category is an empty 200)
//   - empty 200               → NOT degraded: data is a well-formed empty VM, the card shows a
//                               clean empty state (the fail-open requirement in the brief)
import { useCallback, useEffect, useState } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import type { AggregationClient } from '../lib/aggregationClient';

export interface MintSection<T> {
  data: T | null;
  loading: boolean;
  /** false = healthy; otherwise the detail string handed straight to ChartCard's `degraded`. */
  degraded: { detail: string } | false;
  refetch: () => void;
}

const NOT_WIRED_STATUS = new Set([404, 501]);

function classifyError(err: unknown): { detail: string } {
  if (err instanceof Error) {
    const m = /^HTTP (\d+)/.exec(err.message);
    if (m && NOT_WIRED_STATUS.has(Number(m[1]))) return { detail: 'not yet wired' };
    return { detail: err.message };
  }
  return { detail: 'unknown error' };
}

/**
 * Generic single-read MINT section. `call` receives the aggregation client and returns the
 * typed response; `key` is the dependency string that re-fires the read (e.g. the selected
 * category id + metric). A `null` call disables fetching (idle, non-degraded) — used when a
 * section doesn't apply to the current category.
 */
export function useMintSection<T>(
  call: ((client: AggregationClient) => Promise<T>) | null,
  key: string,
): MintSection<T> {
  const [data, setData] = useState<T | null>(null);
  const [loading, setLoading] = useState(call !== null);
  const [degraded, setDegraded] = useState<{ detail: string } | false>(false);

  const fetchOnce = useCallback(() => {
    if (call === null) {
      setLoading(false);
      setData(null);
      setDegraded(false);
      return;
    }
    setLoading(true);
    call(getAggregationClient())
      .then(d => {
        setDegraded(false);
        setData(d);
        setLoading(false);
      })
      .catch(err => {
        setDegraded(classifyError(err));
        setData(null);
        setLoading(false);
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  useEffect(() => {
    let cancelled = false;
    if (call === null) {
      setLoading(false);
      setData(null);
      setDegraded(false);
      return;
    }
    setLoading(true);
    call(getAggregationClient())
      .then(d => { if (!cancelled) { setDegraded(false); setData(d); setLoading(false); } })
      .catch(err => { if (!cancelled) { setDegraded(classifyError(err)); setData(null); setLoading(false); } });
    return () => { cancelled = true; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  return { data, loading, degraded, refetch: fetchOnce };
}
