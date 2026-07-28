// CGUI-03 (TERM #526): the rich seven-region Overview module card (guide spec §3.2 + §8).
// Rebuilds the flat CONST-16 card onto the DS primitives shipped by CGUI-01 (StatusPill,
// Badge) and the brand token sheet. Region order is fixed and identical across every
// registered module:
//   1. drag handle ⠿ + kind node-dot (source blue / core violet / endpoint green / cloud
//      amber) + module name (Inter 600, --text-100)
//   2. StatusPill (online/idle/error; ping ring only when online)
//   3. kind+role line — tracked mono, flow role in the kind's accent colour + muted desc
//   4. metric row (CALLS/H · P50 · COST, cost green at $0) + right-aligned tonal cost badge
//   5. last telemetry log line (wired to the real health.detail) — hidden in compact density
//   6. enable toggle (green when on) + fixed-order actions Configure · Logs · ×
//   7. whole-card hover lift (§8 Card interactive, via the .h-card-interactive class)
// The container itself is the guide's "rich card": gradient fill (--grad-card = space-700→
// space-800) + hairline violet border + --inset-hi + --shadow-md, all carried by
// `.h-card-interactive` so region-7's hover (violet-400 border + glow-violet,shadow-lg,
// -2px lift) comes for free.
import { useState } from 'react';
import type { CSSProperties, DragEvent, KeyboardEvent, MouseEvent } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import type { ModuleDescriptor, PanelDescriptor } from '../../lib/moduleRegistry';
import { getPanelsByModule } from '../../lib/moduleRegistry';
import type { HealthStatus } from '../../lib/aggregationClient';
import type { Density } from '../../components/GlobalBar';
import { StatusPill } from '../../components/StatusPill';
import type { PillState } from '../../components/StatusPill';
import { Badge } from '../../components/Badge';
import { MODULE_META, KIND_COLOR, moduleDetailPath } from './moduleMeta';

/** The §3.2 card-state quartet. OverviewPanel derives online/idle/error from live health;
 *  'disabled' is produced locally when the operator flips the region-6 enable toggle off. */
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

/** Card state → DS StatusPill state (§8). 'disabled' shows an inert idle pill labelled "off". */
const PILL_STATE: Record<CardState, PillState> = {
  online: 'online',
  idle: 'idle',
  error: 'error',
  disabled: 'idle',
};

const PILL_LABEL: Record<CardState, string> = {
  online: 'online',
  idle: 'idle',
  error: 'error',
  disabled: 'off',
};

/** Pick the module's most "logs-like" panel for the region-6 Logs action, else its first. */
function logsPanel(panels: PanelDescriptor[]): PanelDescriptor | undefined {
  return panels.find(p => /log|activ|audit|session/i.test(p.id) || /log|activ|audit|session/i.test(p.title)) ?? panels[0];
}

export function ModuleCard({ module, health, state, density, onMove, onRemove, dragHandlers }: ModuleCardProps) {
  // Region-6 enable toggle. Off → the whole card renders as the §3.2 disabled state.
  const [enabled, setEnabled] = useState(true);
  const effState: CardState = enabled ? state : 'disabled';
  const navigate = useNavigate();

  const meta = MODULE_META[module.id];
  const kindColor = KIND_COLOR[meta.kind];
  const panels = getPanelsByModule(module.id);
  const logPanel = logsPanel(panels);
  const compact = density === 'compact';

  // CGUI-04 (TERM #527): drilling into a module opens its reusable DETAIL view ("same shell,
  // deeper zoom", guide-spec §4) — reached from region-6 "Configure" AND from a click on the
  // card body itself. A body click that lands on any interactive/affordance control is left to
  // that control; only "empty" body clicks drill in, so reordering/removing/toggling never
  // accidentally navigates. The exclusion list covers: buttons, links, the enable switch, AND
  // the region-1 reorder drag handle (`.const-modcard__drag`) — the handle is a drag/no-op
  // affordance (review fix: a click on it must NOT navigate).
  const INERT_SELECTOR = 'button, a, [role="switch"], .const-modcard__drag';
  const openDetail = () => navigate(moduleDetailPath(module.id));
  const onCardClick = (e: MouseEvent<HTMLDivElement>) => {
    if (effState === 'disabled') return; // inert card body (region-6 off) — nothing to open
    if ((e.target as HTMLElement).closest(INERT_SELECTOR)) return;
    openDetail();
  };

  // Region-5 telemetry — wired to the real health probe detail (the one live per-module
  // signal the aggregation client exposes). [ok] for healthy/idle, [!!] for error.
  const detail = health?.detail ?? (health?.available ? 'reachable' : 'unknown');
  const logTag = effState === 'error' ? '[!!]' : '[ok]';
  const logColor = effState === 'error' ? 'var(--flux-rose)' : 'var(--flux-green)';
  const logLine = `${module.id}.health ${detail}`;

  const cardStyle: CSSProperties = {
    position: 'relative',
    display: 'flex',
    flexDirection: 'column',
    gap: 'var(--space-2)',
    padding: compact ? 'var(--space-3)' : 'var(--space-4)',
    cursor: 'default',
  };

  const className = [
    'h-card-interactive',
    'const-modcard',
    effState === 'error' ? 'const-modcard--error' : '',
    effState === 'disabled' ? 'const-modcard--disabled' : '',
  ].filter(Boolean).join(' ');

  const handleKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    // Enter/Space on the focused card (no modifier) drills into the detail view — the keyboard
    // equivalent of an "empty body" click. Ignored when it originates on an inner control so a
    // keyboard press on the toggle/actions still does its own thing.
    if (!e.metaKey && !e.ctrlKey && (e.key === 'Enter' || e.key === ' ')) {
      if ((e.target as HTMLElement).closest(INERT_SELECTOR)) return;
      if (effState === 'disabled') return;
      e.preventDefault();
      openDetail();
      return;
    }
    if (!(e.metaKey || e.ctrlKey)) return;
    if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') { e.preventDefault(); onMove(-1); }
    if (e.key === 'ArrowRight' || e.key === 'ArrowDown') { e.preventDefault(); onMove(1); }
  };

  const labelStyle: CSSProperties = {
    fontFamily: 'var(--font-mono)',
    fontSize: 'var(--fs-mono-sm)',
    textTransform: 'uppercase',
    letterSpacing: 'var(--ls-mono)',
    color: 'var(--text-400)',
  };

  return (
    <div
      role="group"
      aria-label={`${module.title} module card, ${effState} — open detail`}
      tabIndex={0}
      onKeyDown={handleKeyDown}
      onClick={onCardClick}
      className={className}
      style={cardStyle}
      draggable={dragHandlers.draggable}
      onDragStart={dragHandlers.onDragStart}
      onDragOver={dragHandlers.onDragOver}
      onDrop={dragHandlers.onDrop}
    >
      {/* Region 1: drag handle + KIND node-dot + module name. Row also carries the region-2
          StatusPill, right-aligned (§3.2 rows 1–2 share the header line). */}
      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)' }}>
        <span
          aria-hidden
          className="const-modcard__drag"
          title="Drag to reorder (or focus + ⌘/Ctrl+arrow)"
          style={{ cursor: 'grab', color: 'var(--text-400)', fontSize: 'var(--fs-body)', lineHeight: 1 }}
        >
          ⠿
        </span>
        {/* Node-dot: 9px + `0 0 8px` glow is intentional DS-parity geometry (matches the
            NodeBadge/StatusPill dot in CGUI-01); adherence-lint px warnings are expected. */}
        <span
          aria-hidden
          title={`${meta.role.toLowerCase()} node`}
          style={{ width: 9, height: 9, borderRadius: '50%', background: kindColor, boxShadow: `0 0 8px ${kindColor}`, flexShrink: 0 }}
        />
        <span
          style={{
            fontFamily: 'var(--font-sans)',
            fontWeight: 'var(--fw-semibold)',
            color: 'var(--text-100)',
            flex: 1,
            overflow: 'hidden',
            textOverflow: 'ellipsis',
            whiteSpace: 'nowrap',
          }}
        >
          {module.title}
        </span>
        {/* Region 2: StatusPill — ping ring only when online (DS default). */}
        <StatusPill state={PILL_STATE[effState]} label={PILL_LABEL[effState]} />
      </div>

      {/* Region 3: kind + role line — tracked mono, role in the kind accent + muted desc. */}
      <div style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', letterSpacing: 'var(--ls-mono)', color: 'var(--text-300)' }}>
        <span style={{ color: kindColor, fontWeight: 'var(--fw-semibold)' }}>{meta.role}</span>
        {' · '}
        {meta.desc}
      </div>

      {/* Region 4: metric row (3 mono figures) + right-aligned tonal cost badge. calls/h + p50
          are not yet exposed per-module → placeholder em-dash figures; COST is a real $0. */}
      <div style={{ display: 'flex', alignItems: 'flex-end', justifyContent: 'space-between', gap: 'var(--space-2)' }}>
        <div style={{ display: 'flex', gap: 'var(--space-4)' }}>
          <div style={{ display: 'flex', flexDirection: 'column', gap: 2 }}>
            <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono)', color: 'var(--text-100)' }}>—</span>
            <span style={labelStyle}>calls/h</span>
          </div>
          <div style={{ display: 'flex', flexDirection: 'column', gap: 2 }}>
            <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono)', color: 'var(--text-100)' }}>—</span>
            <span style={labelStyle}>p50</span>
          </div>
          <div style={{ display: 'flex', flexDirection: 'column', gap: 2 }}>
            <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono)', color: 'var(--flux-green)' }}>$0</span>
            <span style={labelStyle}>cost</span>
          </div>
        </div>
        <Badge tone={meta.free ? 'green' : 'amber'} mono>
          {meta.free ? '$0/day' : 'opt-in'}
        </Badge>
      </div>

      {/* Region 5: last telemetry log line (hidden in compact density, §3.2). */}
      {!compact && (
        <div
          style={{
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--fs-mono-sm)',
            color: 'var(--text-400)',
            overflow: 'hidden',
            textOverflow: 'ellipsis',
            whiteSpace: 'nowrap',
          }}
        >
          <span style={{ color: logColor }}>{logTag}</span> {logLine}
        </div>
      )}

      {/* Region 6: enable toggle (green when on) + fixed-order actions Configure · Logs · × */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          marginTop: 'auto',
          paddingTop: 'var(--space-2)',
          borderTop: 'var(--border-width) solid var(--border)',
        }}
      >
        {/* Enable toggle — a pill switch, green track when on. */}
        {/* Toggle geometry (34×18 track, 12px knob) is intentional DS-parity component
            geometry — same posture as StatusPill/NodeBadge in CGUI-01; adherence-lint px
            warnings on these numeric literals are expected, not a violation. The __toggle
            class keeps this control clickable while the rest of a disabled card is inert. */}
        <button
          type="button"
          role="switch"
          className="const-modcard__toggle"
          aria-checked={enabled}
          aria-label={enabled ? `Disable ${module.title}` : `Enable ${module.title}`}
          onClick={() => setEnabled(e => !e)}
          style={{
            position: 'relative',
            width: 34,
            height: 18,
            flexShrink: 0,
            borderRadius: 'var(--radius-pill)',
            border: 'var(--border-width) solid var(--border)',
            background: enabled ? 'var(--flux-green)' : 'var(--space-500)',
            cursor: 'pointer',
            transition: 'background var(--dur-fast) var(--ease-out)',
            padding: 0,
          }}
        >
          <span
            aria-hidden
            style={{
              position: 'absolute',
              top: 2,
              left: enabled ? 18 : 2,
              width: 12,
              height: 12,
              borderRadius: '50%',
              background: 'var(--text-100)',
              transition: 'left var(--dur-fast) var(--ease-out)',
            }}
          />
        </button>

        {/* Fixed-order actions: Configure · Logs · ×
            CGUI-04: "Configure" now opens the reusable module DETAIL view (§4) — the depth
            surface (metric tiles, position-in-flow, configuration, live log) every module
            reaches — rather than jumping straight to the module's first panel. */}
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-3)' }}>
          <Link to={moduleDetailPath(module.id)} style={{ fontSize: 'var(--fs-xs)', color: 'var(--accent-bright)', textDecoration: 'none' }}>
            Configure
          </Link>
          {logPanel ? (
            <Link to={logPanel.path} style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-300)', textDecoration: 'none' }}>
              Logs
            </Link>
          ) : (
            <span title="No log surface yet" style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-400)' }}>Logs</span>
          )}
          <button
            type="button"
            onClick={onRemove}
            aria-label={`Remove ${module.title} card`}
            title="Remove this card (restore it via '+ Add widget' below)"
            style={{ background: 'none', border: 'none', color: 'var(--text-400)', fontSize: 'var(--fs-body)', lineHeight: 1, cursor: 'pointer', padding: 0 }}
          >
            ×
          </button>
        </div>
      </div>
    </div>
  );
}
