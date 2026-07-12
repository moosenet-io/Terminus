// CONST-04: Sidebar, adapted from harmony-web's grouped nav — but the groups + items are
// derived from the module registry (src/lib/moduleRegistry.ts), not a hardcoded page table.
// A system group only appears once it has at least one available panel.
import { useState } from 'react';
import { NavLink } from 'react-router-dom';
import { getPanelsBySystem } from '../lib/moduleRegistry';

interface SidebarProps {
  username?: string | null;
  onLogout?: () => void;
}

export function Sidebar({ username, onLogout }: SidebarProps = {}) {
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const groups = getPanelsBySystem();

  const toggleGroup = (label: string) => {
    setCollapsed(prev => {
      const next = new Set(prev);
      if (next.has(label)) next.delete(label);
      else next.add(label);
      return next;
    });
  };

  return (
    <nav style={{
      width: 'var(--h-sidebar-w, 220px)',
      background: 'rgba(0,0,0,0.3)',
      borderRight: '1px solid var(--border-subtle)',
      display: 'flex',
      flexDirection: 'column',
      flexShrink: 0,
      overflow: 'hidden',
    }}>
      <div style={{ padding: 'var(--space-4) var(--space-3) var(--space-3)', borderBottom: '1px solid var(--border-subtle)' }}>
        <div style={{ color: 'var(--accent-primary)', fontWeight: 700, fontSize: 18, letterSpacing: '0.02em' }}>
          Constellation
        </div>
        <p style={{ margin: '6px 0 0', color: 'var(--text-tertiary)', fontSize: 11, letterSpacing: '0.04em' }}>
          Control Plane
        </p>
      </div>

      <div style={{ flex: 1, overflow: 'auto', padding: 'var(--space-2) var(--space-1)' }}>
        {groups.map(group => (
          <div key={group.system} style={{ marginBottom: 'var(--space-1)' }}>
            <button
              onClick={() => toggleGroup(group.system)}
              style={{
                width: '100%', textAlign: 'left', background: 'none', border: 'none',
                color: 'var(--text-tertiary)', fontSize: 'var(--text-xs)', fontWeight: 600,
                textTransform: 'uppercase', letterSpacing: '0.08em',
                padding: 'var(--space-1) 10px var(--space-1)', cursor: 'pointer', display: 'flex',
                justifyContent: 'space-between', alignItems: 'center',
              }}
            >
              {group.system}
              <span style={{ fontSize: 8, opacity: 0.6 }}>{collapsed.has(group.system) ? '▶' : '▼'}</span>
            </button>

            {!collapsed.has(group.system) && group.panels.map(panel => (
              <NavLink
                key={panel.id}
                to={panel.path}
                end={panel.path === '/'}
                style={({ isActive }) => ({
                  display: 'flex',
                  alignItems: 'center',
                  gap: 'var(--space-2)',
                  padding: 'var(--space-1) 10px',
                  borderRadius: 'var(--radius-md)',
                  color: isActive ? 'var(--text-accent)' : 'var(--text-secondary)',
                  background: isActive ? 'var(--accent-primary-subtle)' : 'transparent',
                  textDecoration: 'none',
                  fontSize: 'var(--text-sm)',
                  fontWeight: isActive ? 500 : 400,
                  borderLeft: isActive ? '2px solid var(--accent-primary)' : '2px solid transparent',
                  marginBottom: 1,
                  transition: 'all var(--transition-fast)',
                })}
              >
                <span style={{ fontSize: 'var(--text-base)', opacity: 0.8 }}>{panel.icon ?? '•'}</span>
                {panel.title}
              </NavLink>
            ))}
          </div>
        ))}
      </div>

      <div style={{
        padding: 'var(--space-3) var(--space-4)',
        borderTop: '1px solid var(--border-subtle)',
        color: 'var(--text-tertiary)',
        fontSize: 'var(--text-xs)',
        display: 'flex',
        justifyContent: 'space-between',
        alignItems: 'center',
      }}>
        {username ? (
          <>
            <span title={username} style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', maxWidth: 90 }}>{username}</span>
            {onLogout && (
              <button
                onClick={onLogout}
                style={{
                  background: 'none',
                  border: 'none',
                  color: 'var(--text-tertiary)',
                  cursor: 'pointer',
                  fontSize: 'var(--text-xs)',
                  padding: '2px 0',
                  flexShrink: 0,
                }}
              >
                Sign out
              </button>
            )}
          </>
        ) : (
          <span>CONST-04 shell</span>
        )}
      </div>
    </nav>
  );
}
