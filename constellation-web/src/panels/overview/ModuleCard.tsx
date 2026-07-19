// CONST-16: the Overview card canvas' per-module card — the brand guide's seven-region
// anatomy (§3.1), built against today's tokens (CONST-17's token sheet is a value-only swap on
// top, per the item description: "build against the alias layer if 17 hasn't merged").
import { useState } from 'react';
import type { CSSProperties, DragEvent, KeyboardEvent } from 'react';
import { Link } from 'react-router-dom';
import type { ModuleDescriptor } from '../../lib/moduleRegistry';
import { getPanelsByModule } from '../../lib/moduleRegistry';
import type { HealthStatus } from '../../lib/aggregationClient';
import type { Density } from '../../components/GlobalBar';

/** The §2.6 card-state quartet. Only 'online'/'idle' are currently produced by OverviewPanel
 *  (from module availability + the grace window); 'error'/'disabled' are supported here for
 *  forward-compat with panel-level data-fetch errors and an explicit disable action, neither of
 *  which exists yet in this item's scope. */
export type CardState = 'online' | 'idle' | 'error' | 'disabled';

export interface ModuleCardDragHandlers {
  draggable: boolean;
  onDragStart: (e: DragEvent<HTMLDivElement>) => void;
  onDragOver: (e: DragEvent<HTMLDivElement>) => void;
  onDrop: (e: DragEvent<HTMLDivElement>) => void;
}

interface ModuleCardProps {
  module: ModuleDescriptor;
  health?: HealthStatus;
  state: CardState;
  density: Density;
  onMove: (direction: -1 | 1) => void;
  onRemove: () => void;
  dragHandlers: ModuleCardDragHandlers;
}

const STATE_BORDER: Record<CardState, string> = {
  online: 'var(--border-subtle)',
  idle: 'var(--border-subtle)',
  error: 'var(--status-error)',
  disabled: 'var(--border-subtle)',
};

const STATE_DOT: Record<CardState, string> = {
  online: 'var(--status-success)',
  idle: 'var(--text-tertiary)',
  error: 'var(--status-error)',
  disabled: 'var(--text-tertiary)',
};

export function ModuleCard({ module, health, state, density, onMove, onRemove, dragHandlers }: ModuleCardProps) {
  const [expanded, setExpanded] = useState(false);
  const panels = getPanelsByModule(module.id);
  const firstPanel = panels[0];
  const compact = density === 'compact';

  const style: CSSProperties = {
    background: 'var(--bg-surface)',
    border: `1px solid ${STATE_BORDER[state]}`,
    borderRadius: 'var(--radius-lg)',
    boxShadow: 'var(--shadow-card)',
    padding: compact ? 'var(--space-3)' : 'var(--space-4)',
    display: 'flex',
    flexDirection: 'column',
    gap: 'var(--space-2)',
    opacity: state === 'disabled' ? 0.5 : 1,
  };

  const handleKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (!(e.metaKey || e.ctrlKey)) return;
    if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') {
      e.preventDefault();
      onMove(-1);
    }
    if (e.key === 'ArrowRight' || e.key === 'ArrowDown') {
      e.preventDefault();
      onMove(1);
    }
  };

  return (
    <div
      role="group"
      aria-label={`${module.title} module card, ${state}`}
      tabIndex={0}
      onKeyDown={handleKeyDown}
      style={style}
      draggable={dragHandlers.draggable}
      onDragStart={dragHandlers.onDragStart}
      onDragOver={dragHandlers.onDragOver}
      onDrop={dragHandlers.onDrop}
    >
      {/* Region 1: drag handle + semantic node dot + module name */}
      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)' }}>
        <span aria-hidden title="Drag to reorder (or focus + ⌘/Ctrl+arrow)" style={{ cursor: 'grab', color: 'var(--text-tertiary)' }}>
          ⠿
        </span>
        <span aria-hidden style={{ width: 8, height: 8, borderRadius: '50%', background: STATE_DOT[state], flexShrink: 0 }} />
        <span style={{ fontWeight: 600, color: 'var(--text-primary)', flex: 1, overflow: 'hidden', textOverflow: 'ellipsis' }}>
          {module.icon} {module.title}
        </span>
        <button
          onClick={() => setExpanded(e => !e)}
          aria-expanded={expanded}
          aria-label={expanded ? 'Collapse card' : 'Expand card'}
          style={{ background: 'none', border: 'none', color: 'var(--text-tertiary)', cursor: 'pointer', fontSize: 'var(--text-xs)' }}
        >
          {expanded ? '▾' : '▸'}
        </button>
      </div>

      {/* Region 2: StatusPill */}
      <div
        style={{
          display: 'inline-flex',
          alignItems: 'center',
          gap: 6,
          alignSelf: 'flex-start',
          fontFamily: 'var(--font-mono)',
          fontSize: 'var(--text-xs)',
          textTransform: 'uppercase',
          letterSpacing: '0.05em',
          color: STATE_DOT[state],
          background: 'var(--bg-surface-raised)',
          padding: '2px 8px',
          borderRadius: 10,
        }}
      >
        <span
          aria-hidden
          style={{
            width: 7,
            height: 7,
            borderRadius: '50%',
            background: STATE_DOT[state],
            animation: state === 'online' ? 'h-pulse 1.8s ease-out infinite' : 'none',
          }}
        />
        {state}
      </div>

      {/* Region 3: kind/role line */}
      <div style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)' }}>
        <span style={{ color: 'var(--accent-primary)' }}>module</span> · {panels.length} panel{panels.length === 1 ? '' : 's'}
      </div>

      {/* Region 4: metric row */}
      <div style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-sm)', color: 'var(--text-primary)' }}>
        status: {health?.detail ?? (health?.available ? 'reachable' : 'unknown')}
      </div>

      {/* Region 5: last activity (hidden in Compact density, §3.1) */}
      {!compact && (
        <div style={{ fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)' }}>last activity: n/a</div>
      )}

      {/* Region 6: enable/hide toggle + quick actions (fixed order: Open · Configure) */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          marginTop: 'auto',
          paddingTop: 'var(--space-2)',
        }}
      >
        <button
          onClick={onRemove}
          title="Hide this card (restore it via '+ Add widget' below)"
          style={{ background: 'none', border: 'none', color: 'var(--text-tertiary)', fontSize: 'var(--text-xs)', cursor: 'pointer' }}
        >
          Hide
        </button>
        <div style={{ display: 'flex', gap: 'var(--space-3)' }}>
          {firstPanel ? (
            <Link to={firstPanel.path} style={{ fontSize: 'var(--text-xs)', color: 'var(--accent-primary)', textDecoration: 'none' }}>
              Open
            </Link>
          ) : (
            <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)' }}>Open</span>
          )}
          <span
            title="No configuration surface yet"
            style={{ fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)', cursor: 'default' }}
          >
            Configure
          </span>
        </div>
      </div>

      {/* Region 7: card body widget when expanded */}
      {expanded && (
        <div
          style={{
            borderTop: '1px solid var(--border-subtle)',
            paddingTop: 'var(--space-2)',
            display: 'flex',
            flexDirection: 'column',
            gap: 4,
          }}
        >
          {panels.length === 0 && (
            <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)' }}>No panels registered yet.</span>
          )}
          {panels.map(p => (
            <Link key={p.id} to={p.path} style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)', textDecoration: 'none' }}>
              {p.icon ?? '•'} {p.title}
            </Link>
          ))}
        </div>
      )}
    </div>
  );
}
