// CONST-25 (§3.2): the full command palette, replacing GlobalBar's CONST-16 `MiniPalette`
// (navigation-only). Sources, in rank order: (1) navigation — every currently-available panel;
// (2) actions — `commandRegistry.ts` entries, role-gated; (3) entity search — async, debounced,
// fanned out through `entitySearch.ts`, grouped per-source, degrading independently. Zero new
// deps: own subsequence matcher (`commandMatch.ts`), own dialog/listbox markup, CSS tokens only.
//
// Ownership: Ctrl/Cmd+K + open/close state live in App.tsx's Shell (so the shortcut works from
// anywhere the shell is mounted, not just while GlobalBar has focus); this component is fully
// controlled (`open`/`onClose`) and stateless about *whether* it's shown, only about its own
// query/selection while it is.
import { useEffect, useMemo, useRef, useState } from 'react';
import type { PanelDescriptor } from '../lib/moduleRegistry';
import { getAggregationClient } from '../lib/aggregationClient';
import { rankItems } from '../lib/commandMatch';
import { getAvailableCommands } from '../lib/commandRegistry';
import type { CommandDescriptor } from '../lib/commandRegistry';
import { searchEntities } from '../lib/entitySearch';
import type { EntitySourceResult } from '../lib/entitySearch';

const ENTITY_DEBOUNCE_MS = 150;

interface PaletteRow {
  key: string;
  group: string;
  label: string;
  sublabel?: string;
  icon?: string;
  degraded?: boolean;
  onSelect: () => void;
}

export interface CommandPaletteProps {
  open: boolean;
  onClose: () => void;
  /** The SAME health-filtered panel set the shell routes — never the raw registry (matches the
   *  MiniPalette convention this replaces, so a dead module's panel can never appear here). */
  panels: PanelDescriptor[];
  onNavigate: (path: string) => void;
  /** `null` until CONST-27's `useAuthRole` lands on main — see `commandRegistry.ts`'s
   *  `getAvailableCommands` doc for the resulting (deliberately operator-default) seam. */
  role: 'operator' | 'viewer' | null;
}

export function CommandPalette({ open, onClose, panels, onNavigate, role }: CommandPaletteProps) {
  const [query, setQuery] = useState('');
  const [activeKey, setActiveKey] = useState<string | null>(null);
  const [entityResults, setEntityResults] = useState<EntitySourceResult[]>([]);
  const inputRef = useRef<HTMLInputElement>(null);
  const debounceRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** Monotonic id per issued entity search — only the LATEST request may apply its
   *  results (review fix: a slower older request must never overwrite a newer query's
   *  hits; the debounce bounds request VOLUME, not result freshness). */
  const searchSeqRef = useRef(0);
  /** The element focused when the palette opened — focus returns there on close
   *  (review fix: keyboard users must resume where they were, per the dialog pattern). */
  const restoreFocusRef = useRef<HTMLElement | null>(null);
  const listboxId = 'command-palette-listbox';

  useEffect(() => {
    if (open) {
      restoreFocusRef.current =
        document.activeElement instanceof HTMLElement ? document.activeElement : null;
      setQuery('');
      setEntityResults([]);
      // Focus after the dialog paints so the browser doesn't fight an in-flight blur.
      const id = setTimeout(() => inputRef.current?.focus(), 0);
      return () => clearTimeout(id);
    }
    // Closed: hand focus back to wherever the user was.
    const prev = restoreFocusRef.current;
    restoreFocusRef.current = null;
    if (prev && document.contains(prev)) prev.focus();
  }, [open]);

  // Entity search: debounced 150ms, degrades per-source (searchEntities never rejects as a
  // whole — see its doc comment). Skipped entirely for an empty query. Stale responses are
  // dropped via searchSeqRef: the sequence advances on EVERY query change, clear, and
  // close (cycle-2 review fix — bumping only when a request was issued let an in-flight
  // older search apply results after the input was cleared or retyped), so an in-flight
  // response is only applied when it still belongs to the CURRENT query state.
  useEffect(() => {
    // Any query-state transition (including close + clear) invalidates whatever is in flight.
    const seq = ++searchSeqRef.current;
    if (!open) return;
    if (debounceRef.current) clearTimeout(debounceRef.current);
    if (query.trim().length === 0) {
      setEntityResults([]);
      return;
    }
    debounceRef.current = setTimeout(() => {
      searchEntities(query, getAggregationClient()).then(results => {
        if (seq === searchSeqRef.current) setEntityResults(results);
      });
    }, ENTITY_DEBOUNCE_MS);
    return () => {
      if (debounceRef.current) clearTimeout(debounceRef.current);
    };
  }, [query, open]);

  const commands = useMemo(() => getAvailableCommands(role), [role]);

  const groups = useMemo(() => {
    const nav: PaletteRow[] =
      (query.trim() === ''
        ? panels.map(p => ({ item: p, label: p.title }))
        : rankItems(query, panels, p => p.title).map(r => ({ item: r.item, label: r.item.title }))
      ).map(({ item }) => ({
        key: `nav.${item.id}`,
        group: 'Go to',
        label: item.title,
        icon: item.icon,
        onSelect: () => onNavigate(item.path),
      }));

    const actionSource: CommandDescriptor[] =
      query.trim() === '' ? commands : rankItems(query, commands, c => `${c.title} ${c.subtitle ?? ''}`).map(r => r.item);
    const actions: PaletteRow[] = actionSource.map(c => ({
      key: `action.${c.id}`,
      group: 'Actions',
      label: c.title,
      sublabel: c.subtitle,
      icon: c.icon,
      onSelect: c.run,
    }));

    const entityGroups: PaletteRow[] = entityResults.flatMap((r): PaletteRow[] =>
      r.status === 'error'
        ? [{ key: `entity-error.${r.group}`, group: r.group, label: `${r.group} unavailable`, sublabel: undefined, degraded: true, onSelect: () => {} }]
        : r.hits.map(hit => ({
            key: `entity.${r.group}.${hit.id}`,
            group: r.group,
            label: hit.label,
            sublabel: hit.sublabel,
            onSelect: () => onNavigate(hit.path),
          })),
    );

    return [{ name: 'Go to', rows: nav }, { name: 'Actions', rows: actions }].concat(
      Object.entries(
        entityGroups.reduce<Record<string, PaletteRow[]>>((acc, row) => {
          (acc[row.group] ??= []).push(row);
          return acc;
        }, {}),
      ).map(([name, rows]) => ({ name, rows })),
    );
  }, [panels, commands, query, entityResults, onNavigate]);

  const flatRows = useMemo(() => groups.flatMap(g => g.rows), [groups]);

  useEffect(() => {
    if (flatRows.length === 0) {
      setActiveKey(null);
      return;
    }
    if (!flatRows.some(r => r.key === activeKey)) setActiveKey(flatRows[0].key);
  }, [flatRows, activeKey]);

  if (!open) return null;

  const activeIndex = flatRows.findIndex(r => r.key === activeKey);

  const move = (delta: number) => {
    if (flatRows.length === 0) return;
    const next = (activeIndex + delta + flatRows.length) % flatRows.length;
    setActiveKey(flatRows[next].key);
  };

  const cycleGroup = (delta: number) => {
    if (groups.every(g => g.rows.length === 0)) return;
    const nonEmpty = groups.filter(g => g.rows.length > 0);
    const currentGroup = nonEmpty.findIndex(g => g.rows.some(r => r.key === activeKey));
    const next = nonEmpty[(currentGroup + delta + nonEmpty.length) % nonEmpty.length];
    setActiveKey(next.rows[0].key);
  };

  const runActive = () => {
    const row = flatRows.find(r => r.key === activeKey);
    if (!row || row.degraded) return;
    row.onSelect();
    onClose();
  };

  return (
    <div
      role="presentation"
      onClick={onClose}
      style={{ position: 'fixed', inset: 0, background: 'rgba(0,0,0,0.5)', display: 'flex', alignItems: 'flex-start', justifyContent: 'center', paddingTop: '10vh', zIndex: 1000 }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label="Command palette"
        onClick={e => e.stopPropagation()}
        style={{ width: 560, maxWidth: '92vw', maxHeight: '70vh', display: 'flex', flexDirection: 'column', background: 'var(--bg-surface)', border: '1px solid var(--border-default)', borderRadius: 'var(--radius-lg)', boxShadow: 'var(--shadow-card-elevated)', overflow: 'hidden' }}
      >
        <input
          ref={inputRef}
          role="combobox"
          aria-expanded="true"
          aria-controls={listboxId}
          aria-autocomplete="list"
          aria-activedescendant={activeKey ?? undefined}
          value={query}
          onChange={e => setQuery(e.target.value)}
          placeholder="Search panels, actions, sessions, agents, providers, models…"
          onKeyDown={e => {
            if (e.key === 'ArrowDown') { e.preventDefault(); move(1); }
            else if (e.key === 'ArrowUp') { e.preventDefault(); move(-1); }
            else if (e.key === 'Tab') { e.preventDefault(); cycleGroup(e.shiftKey ? -1 : 1); }
            else if (e.key === 'Enter') { e.preventDefault(); runActive(); }
            else if (e.key === 'Escape') { e.preventDefault(); onClose(); }
          }}
          style={{ border: 'none', borderBottom: '1px solid var(--border-subtle)', background: 'transparent', color: 'var(--text-primary)', font: 'inherit', fontSize: 'var(--text-base)', padding: 'var(--space-3) var(--space-4)', outline: 'none' }}
        />

        <div id={listboxId} role="listbox" aria-label="Command palette results" style={{ overflowY: 'auto', flex: 1 }}>
          {groups.filter(g => g.rows.length > 0).length === 0 && (
            <div style={{ padding: 'var(--space-4)', color: 'var(--text-tertiary)', fontSize: 'var(--text-sm)' }}>No results.</div>
          )}
          {groups.map(group =>
            group.rows.length === 0 ? null : (
              <div key={group.name}>
                <div style={{ padding: 'var(--space-2) var(--space-4) var(--space-1)', color: 'var(--text-tertiary)', fontSize: 'var(--text-xs)', textTransform: 'uppercase', letterSpacing: '0.08em' }}>
                  {group.name}
                </div>
                {group.rows.map(row => (
                  <div
                    key={row.key}
                    id={row.key}
                    role="option"
                    aria-selected={row.key === activeKey}
                    aria-disabled={row.degraded || undefined}
                    onMouseEnter={() => setActiveKey(row.key)}
                    onClick={() => { if (!row.degraded) { row.onSelect(); onClose(); } }}
                    style={{
                      display: 'flex', alignItems: 'baseline', gap: 'var(--space-2)',
                      padding: 'var(--space-2) var(--space-4)', cursor: row.degraded ? 'default' : 'pointer',
                      background: row.key === activeKey ? 'var(--accent-primary-subtle)' : 'transparent',
                      color: row.degraded ? 'var(--text-tertiary)' : 'var(--text-primary)',
                      fontStyle: row.degraded ? 'italic' : 'normal',
                    }}
                  >
                    {row.icon && <span aria-hidden>{row.icon}</span>}
                    <span style={{ flex: 1 }}>{row.label}</span>
                    {row.sublabel && <span style={{ color: 'var(--text-tertiary)', fontSize: 'var(--text-xs)' }}>{row.sublabel}</span>}
                  </div>
                ))}
              </div>
            ),
          )}
        </div>

        <div style={{ padding: 'var(--space-2) var(--space-4)', borderTop: '1px solid var(--border-subtle)', color: 'var(--text-tertiary)', fontSize: 'var(--text-xs)', display: 'flex', gap: 'var(--space-3)' }}>
          <span>↑↓ navigate</span>
          <span>Tab cycle groups</span>
          <span>Enter select</span>
          <span>Esc close</span>
        </div>
      </div>
    </div>
  );
}
