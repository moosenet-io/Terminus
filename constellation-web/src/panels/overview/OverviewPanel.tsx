// CONST-16: the Overview card canvas (§3.1, default route `/overview`) — one seven-region
// ModuleCard per available module. Drag-reorder + remove + "+ Add widget" restore, with a
// ⌘/Ctrl+arrow keyboard equivalent (handled per-card in ModuleCard). Layout + density persist
// ONLY through `client.prefs` (the allowlisted localStorage seam in aggregationClient.ts) —
// this file never touches `localStorage` directly.
import { useMemo, useState } from 'react';
import type { DragEvent } from 'react';
import { NavLink } from 'react-router-dom';
import type { ModuleDescriptor, ModuleId } from '../../lib/moduleRegistry';
import { getPanelsByModule } from '../../lib/moduleRegistry';
import type { HealthStatus } from '../../lib/aggregationClient';
import { getAggregationClient } from '../../lib/aggregationClient';
import type { Density } from '../../components/GlobalBar';
import type { CoreDescriptor } from '../../lib/cores';
import { Button } from '../../components/Button';
import { EmptyState } from '../../components/EmptyState';
import { ModuleCard } from './ModuleCard';
import type { CardState } from './ModuleCard';
import { ActivityFeedCard } from './ActivityFeedCard';
import type { FeedItem } from '../../lib/activityFeed';
import { KIND_COLOR, MODULE_META } from './moduleMeta';

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
  /** S127 (§3.1): the active core — names the breadcrumb + canvas title, and (upstream) has
   *  already scoped `modules` to its members. Optional so any test/story that predates the core
   *  model keeps compiling; falls back to an "overview" heading with no core name. */
  core?: CoreDescriptor;
  modules: ModuleDescriptor[];
  health: HealthStatus[];
  degradedSystems: Set<string>;
  density: Density;
  /** CONST-26 (§3.3): the shell's merged activity feed — renders as a fixed extra card in this
   *  canvas (see `ActivityFeedCard`'s doc for why it's not a reorderable `ModuleCard`). Optional
   *  so this panel keeps working for any test/story that doesn't pass one. */
  feedItems?: FeedItem[];
}

export function OverviewPanel({ core, modules, health, degradedSystems, density, feedItems }: OverviewPanelProps) {
  const client = useMemo(() => getAggregationClient(), []);
  const [layout, setLayout] = useState<LayoutPrefs>(
    () => client.prefs.get<LayoutPrefs>('layout') ?? DEFAULT_LAYOUT,
  );
  const [dragId, setDragId] = useState<string | null>(null);
  // CGUI-12: the "Add to canvas" widget tray + edit affordances are a client-side view toggle
  // (there is no layout-editing backend beyond the `client.prefs` layout the cards already
  // persist). "Edit layout" toggles it; "+ Add widget" opens it.
  const [editing, setEditing] = useState(false);

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
    // POL-10: composed empty state instead of a bare centered string.
    return (
      <div style={{ flex: 1, display: 'flex', alignItems: 'center', justifyContent: 'center' }}>
        <EmptyState
          title={core ? `${core.title} is not available` : 'No modules available'}
          message="No modules in this core are reporting healthy right now. They reappear here automatically once their health probe recovers."
          tone="var(--status-warning)"
        />
      </div>
    );
  }

  const coreName = core?.title ?? 'overview';

  // POL-03: a composed core-status strip so even a single-member core (Lumina) reads as a
  // dashboard — core state header + key metrics — rather than one lonely card in a void.
  const onlineCount = modules.filter(m => health.find(h => h.system === m.healthSystem)?.available !== false && !degradedSystems.has(m.healthSystem)).length;
  const degradedCount = modules.filter(m => degradedSystems.has(m.healthSystem)).length;
  const summaryTiles: { label: string; value: string; tone?: string }[] = [
    { label: 'modules', value: String(modules.length) },
    { label: 'online', value: String(onlineCount), tone: 'var(--flux-green)' },
    { label: 'degraded', value: String(degradedCount), tone: degradedCount ? 'var(--status-warning)' : 'var(--text-100)' },
    { label: 'spend today', value: '$0.00', tone: 'var(--flux-green-soft)' },
  ];

  // POL-03: a quick-access panel grid for the core's members — fills short pages with real,
  // useful composition (a deep-link into every panel) instead of dead space below the cards.
  const quickPanels = modules.flatMap(m =>
    getPanelsByModule(m.id).map(p => ({ moduleId: m.id, moduleTitle: m.title, ...p })),
  );

  return (
    <div style={{ padding: 'var(--space-5)', overflow: 'auto', flex: 1 }}>
      {/* Canvas header (§3.1): breadcrumb `{core} / overview` + title + Edit layout (ghost) +
          "+ Add widget" (primary). */}
      <div
        style={{
          display: 'flex',
          alignItems: 'flex-end',
          justifyContent: 'space-between',
          gap: 'var(--space-4)',
          flexWrap: 'wrap',
          marginBottom: 'var(--space-5)',
        }}
      >
        <div style={{ minWidth: 0 }}>
          <div
            style={{
              fontFamily: 'var(--font-mono)',
              fontSize: 'var(--fs-mono-sm)',
              letterSpacing: 'var(--ls-mono)',
              color: 'var(--text-400)',
              marginBottom: 'var(--space-1)',
            }}
          >
            {coreName} <span style={{ color: 'var(--text-500)' }}>/</span> overview
          </div>
          <h1
            style={{
              margin: 0,
              fontFamily: 'var(--font-sans)',
              fontSize: 'var(--fs-h3)',
              fontWeight: 'var(--fw-semibold)',
              color: 'var(--text-100)',
              lineHeight: 'var(--lh-heading)',
            }}
          >
            {coreName}
          </h1>
        </div>
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', flexShrink: 0 }}>
          <Button variant="ghost" size="sm" aria-pressed={editing} onClick={() => setEditing(e => !e)}>
            Edit layout
          </Button>
          <Button variant="primary" size="sm" onClick={() => setEditing(true)}>
            + Add widget
          </Button>
        </div>
      </div>

      {/* POL-03: core-status summary strip — key metrics that fill the top band and make the
          page read as a composed dashboard rather than a sparse card cluster. */}
      <div
        style={{
          display: 'grid',
          gridTemplateColumns: 'repeat(auto-fit, minmax(140px, 1fr))',
          border: 'var(--border-width) solid var(--line-default)',
          borderRadius: 'var(--radius-lg)',
          background: 'var(--grad-card)',
          boxShadow: 'var(--shadow-sm)',
          overflow: 'hidden',
          marginBottom: 'var(--space-5)',
        }}
      >
        {summaryTiles.map((t, i) => (
          <div key={t.label} style={{ padding: 'var(--space-4)', borderLeft: i === 0 ? undefined : 'var(--border-width) solid var(--line-soft)' }}>
            <div style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-h4)', color: t.tone ?? 'var(--text-100)' }}>{t.value}</div>
            <div style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', letterSpacing: 'var(--ls-mono)', textTransform: 'uppercase', color: 'var(--text-500)', marginTop: 'var(--space-2)' }}>{t.label}</div>
          </div>
        ))}
      </div>

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
          // §3.2 states: a FAILING probe renders the rose 'error' state. Backend failure
          // details are free-form strings ("chord probe timed out", "upstream status 503",
          // "chord unreachable: …") — none reliably prefixed "error" — so we key off
          // availability/degradation, never a string prefix (CGUI-03 review FIX B). A module
          // reaches this canvas either healthy (available) or failing-within-grace (App forces
          // available:true during the grace window but flags it in `degradedSystems`); both a
          // raw available:false and grace-window degradation mean the probe is failing.
          // Otherwise 'online'. ('idle' and 'disabled' are card-supported states — 'disabled'
          // is produced by the card's own enable toggle — but the Overview derives only
          // online/error from live health.)
          const failing = h?.available === false || degradedSystems.has(mod.healthSystem);
          const state: CardState = failing ? 'error' : 'online';
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
        {feedItems && <ActivityFeedCard items={feedItems} />}
      </div>

      {/* POL-03: quick-access panel grid — deep-links into every panel of the core's members.
          This is the composition that fills a short/single-member core page (e.g. Lumina) so no
          page reads as mostly-empty; the row dots reuse the module's kind node-dot colour. */}
      {quickPanels.length > 0 && (
        <div style={{ marginTop: 'var(--space-6)' }}>
          <div
            style={{
              fontFamily: 'var(--font-mono)',
              fontSize: 'var(--fs-label)',
              textTransform: 'uppercase',
              letterSpacing: 'var(--ls-label)',
              color: 'var(--text-400)',
              marginBottom: 'var(--space-3)',
            }}
          >
            Panels
          </div>
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(220px, 1fr))', gap: 'var(--space-3)' }}>
            {quickPanels.map(p => {
              const dot = KIND_COLOR[MODULE_META[p.moduleId as ModuleId].kind];
              return (
                <NavLink
                  key={p.id}
                  to={p.path}
                  style={{
                    display: 'flex',
                    alignItems: 'center',
                    gap: 'var(--space-3)',
                    padding: 'var(--space-3) var(--space-4)',
                    border: 'var(--border-width) solid var(--line-default)',
                    borderRadius: 'var(--radius-md)',
                    background: 'var(--bg-surface)',
                    textDecoration: 'none',
                    color: 'var(--text-200)',
                  }}
                >
                  <span aria-hidden style={{ width: 7, height: 7, borderRadius: '50%', background: dot, flexShrink: 0 }} />
                  <span style={{ display: 'flex', flexDirection: 'column', minWidth: 0 }}>
                    <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-100)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{p.title}</span>
                    <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', letterSpacing: 'var(--ls-mono)', color: 'var(--text-500)' }}>{p.moduleTitle}</span>
                  </span>
                </NavLink>
              );
            })}
          </div>
        </div>
      )}

      {/* "Add to canvas" widget tray (§3.1) — the modules removed from the canvas, restorable
          with a `+`. Opened by "Edit layout" / "+ Add widget". Empty-state string is the guide's
          "Every module is on the canvas." */}
      {editing && (
        <div
          className="h-card"
          style={{
            marginTop: 'var(--space-5)',
            padding: 'var(--space-4)',
            display: 'flex',
            flexDirection: 'column',
            gap: 'var(--space-3)',
          }}
        >
          <div
            style={{
              fontFamily: 'var(--font-mono)',
              fontSize: 'var(--fs-label)',
              textTransform: 'uppercase',
              letterSpacing: 'var(--ls-label)',
              color: 'var(--text-400)',
            }}
          >
            Add to canvas
          </div>
          {hiddenIds.length === 0 ? (
            <div style={{ color: 'var(--text-tertiary)', fontSize: 'var(--text-sm)' }}>
              Every module is on the canvas.
            </div>
          ) : (
            <div style={{ display: 'flex', gap: 'var(--space-2)', flexWrap: 'wrap' }}>
              {hiddenIds.map(id => {
                const mod = modules.find(m => m.id === (id as ModuleId));
                if (!mod) return null;
                return (
                  <button
                    key={id}
                    onClick={() => addCard(id)}
                    style={{
                      display: 'flex',
                      alignItems: 'center',
                      gap: 'var(--space-2)',
                      background: 'var(--bg-surface)',
                      border: '1px dashed var(--border-default)',
                      color: 'var(--text-secondary)',
                      borderRadius: 'var(--radius-md)',
                      padding: 'var(--space-1) var(--space-3)',
                      fontSize: 'var(--text-sm)',
                      cursor: 'pointer',
                    }}
                  >
                    <span aria-hidden style={{ color: 'var(--accent-bright)' }}>
                      +
                    </span>
                    {mod.title}
                  </button>
                );
              })}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
