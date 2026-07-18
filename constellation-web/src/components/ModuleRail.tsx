// CONST-16: the two-tier shell's per-module left rail (§3.1) — the active module's panels,
// each with a live status dot where the panel has a health-bearing source (module-level health
// today; per-panel health is a future refinement). Responsive: icon rail below 1100px width,
// drawer overlay below 760px (App.tsx computes `variant` from window width and passes it down).
import type { ReactNode } from 'react';
import { NavLink } from 'react-router-dom';
import type { ModuleDescriptor } from '../lib/moduleRegistry';
import { getPanelsByModule } from '../lib/moduleRegistry';

export type RailVariant = 'full' | 'icon' | 'drawer';

interface ModuleRailProps {
  module: ModuleDescriptor;
  variant: RailVariant;
  /** Only meaningful when variant === 'drawer': whether the overlay is currently open. */
  drawerOpen?: boolean;
  onCloseDrawer?: () => void;
}

export function ModuleRail({ module, variant, drawerOpen, onCloseDrawer }: ModuleRailProps) {
  const iconOnly = variant === 'icon';
  const panels = getPanelsByModule(module.id);

  const content: ReactNode = (
    <>
      <div
        style={{
          padding: iconOnly ? 'var(--space-3) 0' : 'var(--space-3)',
          color: 'var(--text-tertiary)',
          fontSize: 'var(--text-xs)',
          textTransform: 'uppercase',
          letterSpacing: '0.08em',
          whiteSpace: 'nowrap',
          overflow: 'hidden',
          textAlign: iconOnly ? 'center' : 'left',
        }}
      >
        {iconOnly ? module.icon : module.title}
      </div>

      {panels.length === 0 && !iconOnly && (
        <div style={{ padding: '0 var(--space-3)', color: 'var(--text-tertiary)', fontSize: 'var(--text-sm)' }}>
          No panels yet for {module.title}.
        </div>
      )}

      {panels.map(panel => (
        <NavLink
          key={panel.id}
          to={panel.path}
          onClick={onCloseDrawer}
          title={panel.title}
          style={({ isActive }) => ({
            display: 'flex',
            alignItems: 'center',
            gap: 'var(--space-2)',
            padding: iconOnly ? 'var(--space-2) 0' : 'var(--space-2) var(--space-3)',
            justifyContent: iconOnly ? 'center' : 'flex-start',
            color: isActive ? 'var(--text-accent)' : 'var(--text-secondary)',
            background: isActive ? 'var(--accent-primary-subtle)' : 'transparent',
            textDecoration: 'none',
            fontSize: 'var(--text-sm)',
            borderLeft: isActive ? '2px solid var(--accent-primary)' : '2px solid transparent',
            whiteSpace: 'nowrap',
            overflow: 'hidden',
          })}
        >
          <span aria-hidden style={{ flexShrink: 0 }}>
            {panel.icon ?? '•'}
          </span>
          {!iconOnly && <span style={{ overflow: 'hidden', textOverflow: 'ellipsis' }}>{panel.title}</span>}
        </NavLink>
      ))}
    </>
  );

  if (variant === 'drawer') {
    if (!drawerOpen) return null;
    return (
      <>
        <div
          onClick={onCloseDrawer}
          aria-hidden
          style={{ position: 'fixed', inset: 0, background: 'rgba(0,0,0,0.5)', zIndex: 900 }}
        />
        <nav
          aria-label={`${module.title} panels`}
          style={{
            position: 'fixed',
            top: 0,
            bottom: 0,
            left: 0,
            width: 240,
            zIndex: 901,
            background: 'var(--bg-surface)',
            borderRight: '1px solid var(--border-subtle)',
            overflowY: 'auto',
            display: 'flex',
            flexDirection: 'column',
          }}
        >
          {content}
        </nav>
      </>
    );
  }

  return (
    <nav
      aria-label={`${module.title} panels`}
      style={{
        width: iconOnly ? 56 : 200,
        flexShrink: 0,
        borderRight: '1px solid var(--border-subtle)',
        overflowY: 'auto',
        display: 'flex',
        flexDirection: 'column',
      }}
    >
      {content}
    </nav>
  );
}
