// S127 TGUI2 POL-09 (§3.4): a lightweight, tokens-only tab bar for detail/resource views —
// the professional detail-pane pattern (Cloudflare/Grafana resource pages) where a module's
// depth is split across Overview / Config / Logs / Flow tabs rather than one long scroll.
//
// Controlled (activeId / onSelect) so the parent owns which tab is live (and can deep-link to
// one). Real ARIA tablist semantics + arrow-key roving focus; the active tab carries the violet
// accent underline (the ONE accent, per the brand-mute mandate — everything else neutral).
import { useRef } from 'react';
import type { ReactNode } from 'react';

export interface TabItem {
  id: string;
  label: string;
  /** Optional trailing count/badge text (e.g. a log line count) rendered muted+mono. */
  badge?: ReactNode;
}

export interface TabsProps {
  tabs: TabItem[];
  activeId: string;
  onSelect: (id: string) => void;
  /** Namespace for the generated tab/tabpanel DOM ids — MUST match the `idBase` the parent
   *  passes to `tabId`/`tabPanelId` when it renders the tab panels, so `aria-controls` (here)
   *  and `aria-labelledby` (on the panel) reference each other. Keep it unique per Tabs instance
   *  on the page. */
  idBase: string;
  'aria-label'?: string;
}

/** DOM id of a tab BUTTON — the panel's `aria-labelledby` points back here. */
export function tabId(idBase: string, id: string): string {
  return `${idBase}-tab-${id}`;
}

/** DOM id of a tab PANEL — the tab's `aria-controls` points here. */
export function tabPanelId(idBase: string, id: string): string {
  return `${idBase}-panel-${id}`;
}

export function Tabs({ tabs, activeId, onSelect, idBase, 'aria-label': ariaLabel }: TabsProps) {
  const refs = useRef<Record<string, HTMLButtonElement | null>>({});

  const move = (dir: -1 | 1) => {
    const idx = tabs.findIndex(t => t.id === activeId);
    if (idx < 0) return;
    const next = tabs[(idx + dir + tabs.length) % tabs.length];
    onSelect(next.id);
    refs.current[next.id]?.focus();
  };

  return (
    <div
      role="tablist"
      aria-label={ariaLabel}
      style={{
        display: 'flex',
        alignItems: 'stretch',
        gap: 'var(--space-1)',
        borderBottom: 'var(--border-width) solid var(--border-subtle)',
        overflowX: 'auto',
      }}
    >
      {tabs.map(t => {
        const active = t.id === activeId;
        return (
          <button
            key={t.id}
            ref={el => { refs.current[t.id] = el; }}
            id={tabId(idBase, t.id)}
            role="tab"
            aria-selected={active}
            aria-controls={tabPanelId(idBase, t.id)}
            tabIndex={active ? 0 : -1}
            onClick={() => onSelect(t.id)}
            onKeyDown={e => {
              if (e.key === 'ArrowRight') { e.preventDefault(); move(1); }
              else if (e.key === 'ArrowLeft') { e.preventDefault(); move(-1); }
            }}
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 'var(--space-2)',
              background: 'none',
              border: 'none',
              cursor: 'pointer',
              padding: 'var(--space-3) var(--space-3)',
              // The active accent lives on the underline only (M2/M3 — one accent, no ambient
              // fill). Numeric longhands (not a "2px" string literal) keep the DS adherence lint
              // clean while still rendering a 2px accent underline.
              borderBottomWidth: 2,
              borderBottomStyle: 'solid',
              borderBottomColor: active ? 'var(--accent-primary)' : 'transparent',
              marginBottom: -1,
              color: active ? 'var(--text-100)' : 'var(--text-400)',
              fontFamily: 'var(--font-mono)',
              fontSize: 'var(--fs-mono-sm)',
              letterSpacing: 'var(--ls-mono)',
              textTransform: 'uppercase',
              fontWeight: active ? 'var(--fw-semibold)' : 'var(--fw-regular)',
              whiteSpace: 'nowrap',
            }}
          >
            {t.label}
            {t.badge != null && (
              <span style={{ color: 'var(--text-500)', fontSize: 'var(--fs-mono-sm)' }}>{t.badge}</span>
            )}
          </button>
        );
      })}
    </div>
  );
}
