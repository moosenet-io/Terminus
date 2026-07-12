// CONST-04: harmony-web's original App.tsx owned top-level status/WS state and threaded it
// down as props to Dashboard/Projects. In the registry-driven shell, panels are mounted
// standalone (no parent to thread props through), so this hook reproduces that App.tsx-level
// state locally: polls harmony's /status, subscribes to the same WS event stream, and derives
// engine/executor/enrichment state. Both panels/harmony/DashboardPanel.tsx and
// panels/harmony/ProjectsPanel.tsx use it so they see one consistent, live-updating status.
import { useState, useEffect, useCallback } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import { useExecutorState } from './useExecutorState';
import { useWebSocket } from './useWebSocket';
import type { StatusResponse } from '../types/api';
import type { WsEvent } from '../types/events';

export function useHarmonyStatus() {
  const [status, setStatus] = useState<StatusResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [isEnriching, setIsEnriching] = useState(false);

  const { summary: executorSummary, handleEvent: handleExecutorEvent } = useExecutorState();

  const fetchStatus = useCallback(() => {
    setLoading(true);
    setError(null);
    getAggregationClient()
      .request<StatusResponse>('harmony', '/status')
      .then(d => { if (d?.engine_state) setStatus(d); setLoading(false); })
      .catch(e => { setError(e instanceof Error ? e.message : String(e)); setLoading(false); });
  }, []);

  useEffect(() => { fetchStatus(); }, [fetchStatus]);

  // Periodic safety-net poll — matches harmony-web App.tsx's 30s interval.
  useEffect(() => {
    const id = setInterval(fetchStatus, 30000);
    return () => clearInterval(id);
  }, [fetchStatus]);

  const handleWsEvent = useCallback((e: WsEvent) => {
    handleExecutorEvent(e);

    if (e.type === 'enrichment_start') { setIsEnriching(true); return; }
    if (e.type === 'enrichment_done') { setIsEnriching(false); return; }

    if (e.type === 'state_update') {
      const source = e.source ?? '';
      const data = (e.data ?? {}) as Record<string, unknown>;
      if (source === 'engine-state') {
        const execActive = (data.active as boolean | undefined) ?? false;
        const workers = (data.workers as unknown[] | undefined) ?? [];
        setStatus(prev => {
          if (!prev) return prev;
          const vecActive = (prev.vector as { active?: boolean } | undefined)?.active ?? false;
          const eng: StatusResponse['engine_state'] =
            execActive ? (vecActive ? 'EXECUTING/VECTOR' : 'EXECUTING') : (vecActive ? 'EXECUTING/VECTOR' : 'STOPPED');
          return { ...prev, engine_state: eng, workers: workers.length };
        });
      } else if (source === 'tui-state') {
        const projects = data.projects as StatusResponse['projects'] | undefined;
        if (projects?.length) setStatus(prev => (prev ? { ...prev, projects } : prev));
      }
    }
    if (e.type === 'state' && e.data) {
      const raw = e.data as Record<string, unknown>;
      const exec = raw.executor as { active?: boolean; workers?: unknown[] } | undefined;
      const sched = raw.schedule as { uptime_seconds?: number } | undefined;
      const plane = raw.plane as { projects?: StatusResponse['projects'] } | undefined;
      const infMix = (raw.inference_mix as number | undefined) ?? 50;
      setStatus({
        engine_state: exec?.active ? 'EXECUTING' : 'STOPPED',
        workers: exec?.workers?.length ?? 0,
        projects: plane?.projects ?? [],
        inference_mix: infMix,
        uptime_seconds: sched?.uptime_seconds ?? 0,
        verify_score: 'N/A',
      });
    }
  }, [handleExecutorEvent]);

  const { connected } = useWebSocket(handleWsEvent);

  return { status, loading, error, isEnriching, executorSummary, connected, refetch: fetchStatus };
}
