// CGUI-12 (TERM #535): the Overview shell's left module rail (guide-spec §3.1). Where the
// per-module `ModuleRail` lists ONE module's panels (the drill-in view), this rail lists the
// active CRATE's modules grouped by flow role — SOURCES / CORES / ENDPOINTS (/ CLOUD) — each a
// clickable row with a flow-role-coloured status dot, plus the "all systems · $0.00" footer.
// It is the standing frame on `/overview`; clicking a module drills into its detail view.
//
// Responsive: mirrors ModuleRail's variants (full 200px / icon 56px dots-only / drawer overlay),
// driven by the same `variant` App.tsx computes from window width.
import type { ReactNode } from 'react';
import type { ModuleDescriptor } from '../lib/moduleRegistry';
import type { HealthStatus } from '../lib/aggregationClient';
import type { CrateDescriptor } from '../lib/crates';
import { groupByFlowRole } from '../lib/crates';
import { KIND_COLOR } from '../panels/overview/moduleMeta';
import type { RailVariant } from './ModuleRail';

interface CrateModuleRailProps {
  crate: CrateDescriptor;
  /** The active crate's available modules (already filtered by the shell). */
  modules: ModuleDescriptor[];
  health: HealthStatus[];
  /** healthSystem ids inside the stale-while-degrading grace window (dot → amber). */
  degradedSystems: Set<string>;
  /** The module currently drilled into, if any — highlights its rail row. */
  activeModuleId: string | null;
  onSelectModule: (id: string) => void;
  variant: RailVariant;
  drawerOpen?: boolean;
  onCloseDrawer?: () => void;
}

export function CrateModuleRail({
  crate,
  modules,
  health,
  degradedSystems,
  activeModuleId,
  onSelectModule,
  variant,
  drawerOpen,
  onCloseDrawer,
}: CrateModuleRailProps) {
  const iconOnly = variant === 'icon';
  const groups = groupByFlowRole(modules);
  const healthFor = (systemId: string) => health.find(h => h.system === systemId);

  const groupLabelStyle = {
    padding: iconOnly ? 'var(--space-2) 0' : 'var(--space-3) var(--space-3) var(--space-1)',
    fontFamily: 'var(--font-mono)',
    fontSize: 'var(--fs-label)',
    textTransform: 'uppercase' as const,
    // §3.1 group labels are tracked .16em — a guide-specific tracking that sits between the
    // token sheet's --ls-mono (.02em) and --ls-label (.18em); kept as a literal here.
    letterSpacing: '0.16em',
    color: 'var(--text-400)',
    textAlign: iconOnly ? ('center' as const) : ('left' as const),
    whiteSpace: 'nowrap' as const,
    overflow: 'hidden',
  };

  const content: ReactNode = (
    <>
      {/* Crate header — the rail is scoped to one crate, named at the top. */}
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
        {iconOnly ? '◇' : crate.title}
      </div>

      <div style={{ flex: 1, overflowY: 'auto', minHeight: 0 }} className="hf-scroll">
        {groups.length === 0 && !iconOnly && (
          <div style={{ padding: 'var(--space-3)', color: 'var(--text-tertiary)', fontSize: 'var(--text-sm)' }}>
            No modules available in {crate.title}.
          </div>
        )}

        {groups.map(group => (
          <div key={group.label}>
            <div style={groupLabelStyle}>{iconOnly ? '·' : group.label}</div>
            {group.modules.map(m => {
              const h = healthFor(m.healthSystem);
              const degraded = degradedSystems.has(m.healthSystem);
              // Flow-role colour is the module's kind colour (source blue / core violet /
              // endpoint green / cloud amber) — the dot reads "where this module sits in the
              // request path". A degraded (grace-window) module dims to amber; an unavailable
              // one to faint text; otherwise it shows its flow-role colour.
              const dotColor = degraded
                ? 'var(--status-warning)'
                : h?.available
                  ? KIND_COLOR[group.kind]
                  : 'var(--text-tertiary)';
              const active = m.id === activeModuleId;
              return (
                <button
                  key={m.id}
                  type="button"
                  onClick={() => {
                    onSelectModule(m.id);
                    onCloseDrawer?.();
                  }}
                  aria-current={active ? 'page' : undefined}
                  title={degraded ? `${m.title} — degraded (stale-while-recovering)` : m.title}
                  style={{
                    display: 'flex',
                    alignItems: 'center',
                    gap: 'var(--space-2)',
                    width: '100%',
                    background: active ? 'var(--accent-primary-subtle)' : 'transparent',
                    border: 'none',
                    borderLeft: active
                      ? '2px solid var(--accent-primary)'
                      : '2px solid transparent',
                    cursor: 'pointer',
                    padding: iconOnly ? 'var(--space-2) 0' : 'var(--space-2) var(--space-3)',
                    justifyContent: iconOnly ? 'center' : 'flex-start',
                    color: active ? 'var(--text-accent)' : 'var(--text-secondary)',
                    fontSize: 'var(--text-sm)',
                    textAlign: 'left',
                    whiteSpace: 'nowrap',
                    overflow: 'hidden',
                  }}
                >
                  <span
                    aria-hidden
                    style={{
                      width: 7,
                      height: 7,
                      borderRadius: '50%',
                      background: dotColor,
                      boxShadow: `0 0 7px ${dotColor}`,
                      flexShrink: 0,
                    }}
                  />
                  {!iconOnly && (
                    <span style={{ overflow: 'hidden', textOverflow: 'ellipsis' }}>{m.title}</span>
                  )}
                </button>
              );
            })}
          </div>
        ))}
      </div>

      {/* Footer — §3.1 "all systems · $0.00". The $0.00 is load-bearing (every fleet module is
          free/local); the "all systems" reads healthy when nothing is degraded/unavailable. */}
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
          style={{
            width: 6,
            height: 6,
            borderRadius: '50%',
            background: 'var(--status-success)',
            boxShadow: '0 0 7px var(--status-success)',
            flexShrink: 0,
          }}
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
          aria-label={`${crate.title} modules`}
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
      aria-label={`${crate.title} modules`}
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
