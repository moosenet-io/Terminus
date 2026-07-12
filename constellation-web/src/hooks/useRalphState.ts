// SGUI-04: Subscribe to Ralph loop state via WebSocket events
import { useState, useCallback, useEffect, useRef } from 'react';
import type { RalphLoop } from '../types/events';
import type { WsEvent } from '../types/events';

export function useRalphState(): { loops: RalphLoop[]; handleEvent: (e: WsEvent) => void } {
  const [loops, setLoops] = useState<RalphLoop[]>([]);
  // Track completed loops for fade-out (5s)
  const completedTimers = useRef<Map<string, ReturnType<typeof setTimeout>>>(new Map());

  const handleEvent = useCallback((e: WsEvent) => {
    if (e.type === 'ralph_update') {
      const loop = e.data as unknown as RalphLoop;
      setLoops(prev => {
        const idx = prev.findIndex(l => l.id === loop.id);
        if (idx >= 0) {
          const next = [...prev];
          next[idx] = loop;
          return next;
        }
        return [...prev, loop];
      });

      // Schedule fade-out for completed loops
      if (loop.phase === 'done' || loop.phase === 'failed') {
        const timer = setTimeout(() => {
          setLoops(prev => prev.filter(l => l.id !== loop.id));
          completedTimers.current.delete(loop.id);
        }, 5000);
        completedTimers.current.set(loop.id, timer);
      }
    }
    if (e.type === 'state' && e.data) {
      const vector = (e.data as Record<string, unknown>).vector as unknown as RalphLoop | undefined;
      if (vector?.id) {
        setLoops([vector]);
      }
    }
  }, []);

  useEffect(() => {
    return () => {
      completedTimers.current.forEach(t => clearTimeout(t));
    };
  }, []);

  // Show all loops (including 'done'); completedTimers above remove done loops after a delay,
  // so no filter here — the previous `l.phase !== 'done' || true` was a dead no-op.
  return { loops, handleEvent };
}
