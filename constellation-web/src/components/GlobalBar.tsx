// CONST-16: the two-tier shell's top bar (§3.1). Replaces Sidebar as the module switcher —
// module tabs (from `getAvailableModules(health)`, in `order`) carry a health dot; plus the
// wordmark, a ⌘K search/palette trigger, the density toggle, and the account chip.
//
// CONST-25: the ⌘K trigger button here just calls `onOpenPalette` — the palette's own open
// state, keyboard shortcut, and markup live in App.tsx's Shell + `CommandPalette.tsx` now (so
// Ctrl/Cmd+K works everywhere the shell is mounted, not only while this bar has focus). This
// file no longer owns any palette state itself.
import { useNavigate } from 'react-router-dom';
import type { ModuleDescriptor } from '../lib/moduleRegistry';
import type { HealthStatus } from '../lib/aggregationClient';
import { Wordmark } from './Wordmark';

export type Density = 'comfortable' | 'compact';

interface GlobalBarProps {
  modules: ModuleDescriptor[];
  health: HealthStatus[];
  /** healthSystem ids currently inside the 2-cycle stale-while-degrading grace window. */
  degradedSystems: Set<string>;
  activeModuleId: string | null;
  onSelectModule: (id: string) => void;
  density: Density;
  onDensityChange: (d: Density) => void;
  username?: string | null;
  onLogout?: () => void;
  /** True when the last health poll failed outright (network/backend down); the bar shows a
   *  degraded indicator while continuing to render the last known module set (edge case §10). */
  pollDegraded: boolean;
  /** Present only in the <760px "drawer" rail variant — renders a menu trigger before the
   *  wordmark that opens the ModuleRail drawer. */
  onOpenMenu?: () => void;
  /** CONST-25: opens the full CommandPalette (owned by App.tsx's Shell). */
  onOpenPalette: () => void;
}

export function GlobalBar({
  modules,
  health,
  degradedSystems,
  activeModuleId,
  onSelectModule,
  density,
  onDensityChange,
  username,
  onLogout,
  pollDegraded,
  onOpenMenu,
  onOpenPalette,
}: GlobalBarProps) {
  const navigate = useNavigate();

  const healthFor = (systemId: string) => health.find(h => h.system === systemId);

  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 'var(--space-4)',
        padding: '0 var(--space-4)',
        height: 52,
        flexShrink: 0,
        borderBottom: '1px solid var(--border-subtle)',
        background: 'rgba(0,0,0,0.2)',
      }}
    >
      {onOpenMenu && (
        <button
          onClick={onOpenMenu}
          aria-label="Open module navigation"
          style={{
            background: 'none',
            border: '1px solid var(--border-default)',
            borderRadius: 'var(--radius-md)',
            color: 'var(--text-secondary)',
            width: 28,
            height: 28,
            cursor: 'pointer',
            flexShrink: 0,
          }}
        >
          ☰
        </button>
      )}

      <button
        onClick={() => navigate('/overview')}
        style={{ background: 'none', border: 'none', cursor: 'pointer', padding: 0, flexShrink: 0 }}
        aria-label="Go to Overview"
      >
        <Wordmark />
      </button>

      <nav
        aria-label="Modules"
        style={{ display: 'flex', alignItems: 'center', gap: 2, overflowX: 'auto', flex: 1, height: '100%' }}
      >
        {modules.map(m => {
          const h = healthFor(m.healthSystem);
          const active = m.id === activeModuleId;
          const degraded = degradedSystems.has(m.healthSystem);
          const dotColor = degraded
            ? 'var(--status-warning)'
            : h?.available
              ? 'var(--status-success)'
              : 'var(--text-tertiary)';
          return (
            <button
              key={m.id}
              onClick={() => onSelectModule(m.id)}
              aria-current={active ? 'page' : undefined}
              title={degraded ? `${m.title} — degraded (stale-while-recovering)` : m.title}
              style={{
                display: 'flex',
                alignItems: 'center',
                gap: 6,
                background: 'none',
                border: 'none',
                cursor: 'pointer',
                padding: '0 var(--space-3)',
                height: '100%',
                color: active ? 'var(--text-primary)' : 'var(--text-secondary)',
                fontSize: 'var(--text-base)',
                fontWeight: active ? 600 : 400,
                borderBottom: active ? '2px solid var(--accent-primary)' : '2px solid transparent',
                whiteSpace: 'nowrap',
              }}
            >
              <span
                aria-hidden
                style={{ width: 6, height: 6, borderRadius: '50%', background: dotColor, flexShrink: 0 }}
              />
              <span aria-hidden>{m.icon}</span>
              {m.title}
            </button>
          );
        })}
        {modules.length === 0 && (
          <span style={{ color: 'var(--text-tertiary)', fontSize: 'var(--text-sm)' }}>No modules available</span>
        )}
      </nav>

      <button
        onClick={onOpenPalette}
        aria-label="Open command palette"
        style={{
          background: 'var(--bg-surface)',
          border: '1px solid var(--border-default)',
          color: 'var(--text-tertiary)',
          borderRadius: 'var(--radius-md)',
          padding: 'var(--space-1) var(--space-3)',
          fontSize: 'var(--text-sm)',
          cursor: 'pointer',
          flexShrink: 0,
        }}
      >
        search… <kbd style={{ fontFamily: 'var(--font-mono)' }}>⌘K</kbd>
      </button>

      <div
        role="group"
        aria-label="Density"
        style={{
          display: 'flex',
          border: '1px solid var(--border-default)',
          borderRadius: 'var(--radius-md)',
          overflow: 'hidden',
          flexShrink: 0,
        }}
      >
        {(['comfortable', 'compact'] as const).map(d => (
          <button
            key={d}
            onClick={() => onDensityChange(d)}
            aria-pressed={density === d}
            style={{
              padding: 'var(--space-1) var(--space-2)',
              fontSize: 'var(--text-xs)',
              border: 'none',
              cursor: 'pointer',
              textTransform: 'capitalize',
              background: density === d ? 'var(--accent-primary-subtle)' : 'transparent',
              color: density === d ? 'var(--accent-primary)' : 'var(--text-tertiary)',
            }}
          >
            {d}
          </button>
        ))}
      </div>

      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', flexShrink: 0 }}>
        {pollDegraded && (
          <span
            title="Health poll degraded — showing last known status"
            aria-label="Health poll degraded"
            style={{ color: 'var(--status-warning)', fontSize: 'var(--text-sm)' }}
          >
            ⚠
          </span>
        )}
        {username && (
          <span
            title={username}
            style={{
              fontSize: 'var(--text-sm)',
              color: 'var(--text-secondary)',
              maxWidth: 120,
              overflow: 'hidden',
              textOverflow: 'ellipsis',
              whiteSpace: 'nowrap',
            }}
          >
            {username}
          </span>
        )}
        {onLogout && (
          <button
            onClick={onLogout}
            style={{
              background: 'none',
              border: 'none',
              color: 'var(--text-tertiary)',
              cursor: 'pointer',
              fontSize: 'var(--text-xs)',
            }}
          >
            Sign out
          </button>
        )}
      </div>
    </div>
  );
}
