// CONST-16: the Overview card canvas (§3.1, default route `/overview`) — one seven-region
// ModuleCard per available module. Drag-reorder + remove + "+ Add widget" restore, with a
// ⌘/Ctrl+arrow keyboard equivalent (handled per-card in ModuleCard). Layout + density persist
// ONLY through `client.prefs` (the allowlisted localStorage seam in aggregationClient.ts) —
// this file never touches `localStorage` directly.
import { useMemo, useState } from 'react';
import type { DragEvent } from 'react';
import type { ModuleDescriptor, ModuleId } from '../../lib/moduleRegistry';
import type { HealthStatus } from '../../lib/aggregationClient';
import { getAggregationClient } from '../../lib/aggregationClient';
import type { Density } from '../../components/GlobalBar';
import { ModuleCard } from './ModuleCard';
import type { CardState } from './ModuleCard';

/** The `client.prefs` `'layout'` shape — a display order plus a hidden set, both keyed by
 *  ModuleId. Never holds anything else (no widget config, no per-card settings). */
export interface LayoutPrefs {
  order: string[];
  hidden: string[];
}

export const DEFAULT_LAYOUT: LayoutPrefs = { order: [], hidden: [] };

/**
 * Reconciles a persisted layout against the live available-module set: a stale id (module
 * removed/renamed since the layout was saved) is dropped silently; a newly-available module is
 * appended at the end. Exported for unit testing (§10 edge case: "stale persisted layout
 * referencing a removed module → card dropped silently, layout re-saved").
 */
export function reconcileOrder(persistedOrder: string[], availableIds: string[]): string[] {
  const known = new Set(availableIds);
  const kept = persistedOrder.filter(id => known.has(id));
  const missing = availableIds.filter(id => !kept.includes(id));
  return [...kept, ...missing];
}

interface OverviewPanelProps {
  modules: ModuleDescriptor[];
  health: HealthStatus[];
  degradedSystems: Set<string>;
  density: Density;
}

export function OverviewPanel({ modules, health, degradedSystems, density }: OverviewPanelProps) {
  const client = useMemo(() => getAggregationClient(), []);
  const [layout, setLayout] = useState<LayoutPrefs>(
    () => client.prefs.get<LayoutPrefs>('layout') ?? DEFAULT_LAYOUT,
  );
  const [dragId, setDragId] = useState<string | null>(null);

  const availableIds = useMemo(() => modules.map(m => m.id as string), [modules]);
  const orderedIds = useMemo(
    () => reconcileOrder(layout.order, availableIds),
    [layout.order, availableIds],
  );

  const persist = (next: LayoutPrefs) => {
    setLayout(next);
    client.prefs.set('layout', next);
  };

  const hiddenIds = orderedIds.filter(id => layout.hidden.includes(id));
  const visibleIds = orderedIds.filter(id => !layout.hidden.includes(id));

  const moveCard = (id: string, direction: -1 | 1) => {
    const idx = orderedIds.indexOf(id);
    const swapIdx = idx + direction;
    if (idx < 0 || swapIdx < 0 || swapIdx >= orderedIds.length) return;
    const next = [...orderedIds];
    [next[idx], next[swapIdx]] = [next[swapIdx], next[idx]];
    persist({ order: next, hidden: layout.hidden });
  };

  const reorderTo = (draggedId: string, targetId: string) => {
    if (draggedId === targetId) return;
    const next = orderedIds.filter(id => id !== draggedId);
    const targetIdx = next.indexOf(targetId);
    next.splice(targetIdx, 0, draggedId);
    persist({ order: next, hidden: layout.hidden });
  };

  const removeCard = (id: string) => persist({ order: orderedIds, hidden: [...layout.hidden, id] });
  const addCard = (id: string) => persist({ order: orderedIds, hidden: layout.hidden.filter(h => h !== id) });

  if (modules.length === 0) {
    return (
      <div
        style={{
          flex: 1,
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          color: 'var(--text-tertiary)',
          fontSize: 'var(--text-base)',
        }}
      >
        No modules available.
      </div>
    );
  }

  return (
    <div style={{ padding: 'var(--space-5)', overflow: 'auto', flex: 1 }}>
      <div
        style={{
          display: 'grid',
          gridTemplateColumns: 'repeat(auto-fill, minmax(260px, 1fr))',
          gap: 'var(--space-4)',
        }}
      >
        {visibleIds.map(id => {
          const mod = modules.find(m => m.id === (id as ModuleId));
          if (!mod) return null;
          const h = health.find(x => x.system === mod.healthSystem);
          const state: CardState = degradedSystems.has(mod.healthSystem) ? 'idle' : 'online';
          return (
            <ModuleCard
              key={id}
              module={mod}
              health={h}
              state={state}
              density={density}
              onMove={dir => moveCard(id, dir)}
              onRemove={() => removeCard(id)}
              dragHandlers={{
                draggable: true,
                onDragStart: (e: DragEvent<HTMLDivElement>) => {
                  e.dataTransfer.effectAllowed = 'move';
                  setDragId(id);
                },
                onDragOver: (e: DragEvent<HTMLDivElement>) => e.preventDefault(),
                onDrop: (e: DragEvent<HTMLDivElement>) => {
                  e.preventDefault();
                  if (dragId) reorderTo(dragId, id);
                  setDragId(null);
                },
              }}
            />
          );
        })}
      </div>

      {hiddenIds.length > 0 && (
        <div style={{ marginTop: 'var(--space-4)', display: 'flex', gap: 'var(--space-2)', flexWrap: 'wrap' }}>
          {hiddenIds.map(id => {
            const mod = modules.find(m => m.id === (id as ModuleId));
            if (!mod) return null;
            return (
              <button
                key={id}
                onClick={() => addCard(id)}
                style={{
                  background: 'var(--bg-surface)',
                  border: '1px dashed var(--border-default)',
                  color: 'var(--text-tertiary)',
                  borderRadius: 'var(--radius-md)',
                  padding: 'var(--space-1) var(--space-3)',
                  fontSize: 'var(--text-sm)',
                  cursor: 'pointer',
                }}
              >
                + Add widget · {mod.title}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}
