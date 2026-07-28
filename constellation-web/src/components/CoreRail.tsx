// S127 TGUI2 (Part A): the two-tier shell's left rail (guide-spec §3.1), rewritten from
// CrateModuleRail. Level 1 is the 5 core tabs in the GlobalBar; this is Level 2 — the active
// CORE's panels.
//
// - Single-member core (Lumina/Chord/Harmony/Muse): a flat list of that module's panels — the
//   pre-CGUI-12 ModuleRail behaviour (panels, navigating to their real panel path).
// - Multi-member core (Terminus): one labelled sub-group per member module — TERMINUS / MODELS /
//   MINT — each listing that member's panels, so MINT + Models read as Terminus subsections (the
//   two-level hierarchy the operator asked for).
//
// Rows navigate to `panel.path` (NavLink), so the rail behaves like standard sub-nav and the
// active panel is highlighted by the router. Responsive: full 220px / icon 56px dots-only /
// drawer overlay, mirroring ModuleRail's variants.
import type { ReactNode } from 'react';
import { NavLink } from 'react-router-dom';
import type { ModuleDescriptor } from '../lib/moduleRegistry';
import { getPanelsByModule } from '../lib/moduleRegistry';
import type { HealthStatus } from '../lib/aggregationClient';
import type { CoreDescriptor } from '../lib/cores';
import { MEMBER_LABEL } from '../lib/cores';
import { KIND_COLOR, MODULE_META } from '../panels/overview/moduleMeta';
import type { RailVariant } from './ModuleRail';

interface CoreRailProps {
  core: CoreDescriptor;
  /** The active core's available member modules (already filtered by the shell), in core order. */
  modules: ModuleDescriptor[];
  health: HealthStatus[];
  /** healthSystem ids inside the stale-while-degrading grace window (dot → amber). */
  degradedSystems: Set<string>;
  variant: RailVariant;
  drawerOpen?: boolean;
  onCloseDrawer?: () => void;
}

export function CoreRail({
  core,
  modules,
  health,
  degradedSystems,
  variant,
  drawerOpen,
  onCloseDrawer,
}: CoreRailProps) {
  const iconOnly = variant === 'icon';
  const multiMember = core.moduleIds.length > 1;
  const healthFor = (systemId: string) => health.find(h => h.system === systemId);

  const groupLabelStyle = {
    padding: iconOnly ? 'var(--space-2) 0' : 'var(--space-3) var(--space-3) var(--space-1)',
    fontFamily: 'var(--font-mono)',
    fontSize: 'var(--fs-label)',
    textTransform: 'uppercase' as const,
    // §3.1 group labels are tracked .16em — sits between --ls-mono (.02em) and --ls-label (.18em).
    letterSpacing: '0.16em',
    color: 'var(--text-400)',
    textAlign: iconOnly ? ('center' as const) : ('left' as const),
    whiteSpace: 'nowrap' as const,
    overflow: 'hidden',
  };

  /** A member module's panel rows, with a status dot in the member's kind colour. */
  const memberRows = (m: ModuleDescriptor): ReactNode => {
    const panels = getPanelsByModule(m.id);
    const h = healthFor(m.healthSystem);
    const degraded = degradedSystems.has(m.healthSystem);
    const dotColor = degraded
      ? 'var(--status-warning)'
      : h?.available
        ? KIND_COLOR[MODULE_META[m.id].kind]
        : 'var(--text-tertiary)';
    if (panels.length === 0 && !iconOnly) {
      return (
        <div style={{ padding: '0 var(--space-3) var(--space-2)', color: 'var(--text-tertiary)', fontSize: 'var(--text-sm)' }}>
          No panels yet.
        </div>
      );
    }
    return panels.map(panel => (
      <NavLink
        key={panel.id}
        to={panel.path}
        onClick={onCloseDrawer}
        title={panel.title}
        style={({ isActive }) => ({
          display: 'flex',
          alignItems: 'center',
          gap: 'var(--space-2)',
          width: '100%',
          padding: iconOnly ? 'var(--space-2) 0' : 'var(--space-2) var(--space-3)',
          justifyContent: iconOnly ? 'center' : 'flex-start',
          color: isActive ? 'var(--text-accent)' : 'var(--text-secondary)',
          background: isActive ? 'var(--accent-primary-subtle)' : 'transparent',
          borderLeft: isActive ? '2px solid var(--accent-primary)' : '2px solid transparent',
          textDecoration: 'none',
          fontSize: 'var(--text-sm)',
          whiteSpace: 'nowrap',
          overflow: 'hidden',
        })}
      >
        <span
          aria-hidden
          style={{ width: 7, height: 7, borderRadius: '50%', background: dotColor, boxShadow: `0 0 7px ${dotColor}`, flexShrink: 0 }}
        />
        {!iconOnly && <span style={{ overflow: 'hidden', textOverflow: 'ellipsis' }}>{panel.title}</span>}
      </NavLink>
    ));
  };

  const content: ReactNode = (
    <>
      {/* Core header — the rail is scoped to one core, named at the top. */}
      <div
        style={{
          padding: iconOnly ? 'var(--space-3) 0' : 'var(--space-3)',
          fontFamily: 'var(--font-mono)',
          fontSize: 'var(--fs-mono-sm)',
          letterSpacing: 'var(--ls-mono)',
          color: 'var(--text-300)',
          textAlign: iconOnly ? 'center' : 'left',
          whiteSpace: 'nowrap',
          overflow: 'hidden',
          borderBottom: 'var(--border-width) solid var(--border-subtle)',
        }}
      >
        {iconOnly ? '◇' : core.title}
      </div>

      <div style={{ flex: 1, overflowY: 'auto', minHeight: 0 }} className="hf-scroll">
        {modules.length === 0 && !iconOnly && (
          <div style={{ padding: 'var(--space-3)', color: 'var(--text-tertiary)', fontSize: 'var(--text-sm)' }}>
            {core.title} is not available.
          </div>
        )}

        {multiMember
          ? // Terminus: one labelled sub-group per member (TERMINUS / MODELS / MINT).
            modules.map(m => (
              <div key={m.id}>
                <div style={groupLabelStyle}>{iconOnly ? '·' : MEMBER_LABEL[m.id]}</div>
                {memberRows(m)}
              </div>
            ))
          : // Single-member core: a flat panel list (no sub-group header).
            modules.map(m => <div key={m.id}>{memberRows(m)}</div>)}
      </div>

      {/* Footer — §3.1 "all systems · $0.00". Every fleet module is free/local. */}
      <div
        style={{
          padding: iconOnly ? 'var(--space-2) 0' : 'var(--space-3)',
          borderTop: 'var(--border-width) solid var(--border-subtle)',
          fontFamily: 'var(--font-mono)',
          fontSize: 'var(--fs-mono-sm)',
          letterSpacing: 'var(--ls-mono)',
          color: 'var(--text-400)',
          display: 'flex',
          alignItems: 'center',
          gap: 'var(--space-2)',
          justifyContent: iconOnly ? 'center' : 'flex-start',
          whiteSpace: 'nowrap',
          overflow: 'hidden',
        }}
      >
        <span
          aria-hidden
          style={{ width: 6, height: 6, borderRadius: '50%', background: 'var(--status-success)', boxShadow: '0 0 7px var(--status-success)', flexShrink: 0 }}
        />
        {!iconOnly && (
          <span>
            all systems <span style={{ color: 'var(--flux-green)' }}>· $0.00</span>
          </span>
        )}
      </div>
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
          aria-label={`${core.title} panels`}
          style={{
            position: 'fixed',
            top: 0,
            bottom: 0,
            left: 0,
            width: 240,
            zIndex: 901,
            background: 'var(--bg-surface)',
            borderRight: 'var(--border-width) solid var(--border-subtle)',
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
      aria-label={`${core.title} panels`}
      style={{
        width: iconOnly ? 56 : 220,
        flexShrink: 0,
        borderRight: 'var(--border-width) solid var(--border-subtle)',
        display: 'flex',
        flexDirection: 'column',
        minHeight: 0,
      }}
    >
      {content}
    </nav>
  );
}
