// SGUI-03: Subscribe to executor state via WebSocket events
import { useState, useCallback } from 'react';
import type { Worker } from '../types/api';
import type { WsEvent } from '../types/events';

export interface ExecutorSummary {
  workers: Worker[];
  activeCount: number;
  waitingCount: number;
  idleCount: number;
}

export function useExecutorState(): { summary: ExecutorSummary; handleEvent: (e: WsEvent) => void } {
  const [workers, setWorkers] = useState<Worker[]>([]);

  const handleEvent = useCallback((e: WsEvent) => {
    if (e.type === 'state_update' && e.source === 'engine-state') {
      const data = e.data as { active?: boolean; workers?: Worker[] };
      setWorkers(data.workers || []);
    }
    if (e.type === 'state' && e.data) {
      const exec = (e.data as Record<string, unknown>).executor as { workers?: Worker[] } | undefined;
      if (exec?.workers) setWorkers(exec.workers);
    }
  }, []);

  const summary: ExecutorSummary = {
    workers: workers.slice().sort((a, b) => {
      const order = { working: 0, waiting: 1, stalled: 2, failed: 3, idle: 4 };
      return (order[a.status] ?? 5) - (order[b.status] ?? 5);
    }),
    activeCount: workers.filter(w => w.status === 'working').length,
    waitingCount: workers.filter(w => w.status === 'waiting').length,
    idleCount: workers.filter(w => w.status === 'idle').length,
  };

  return { summary, handleEvent };
}
