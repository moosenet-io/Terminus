// CONST-17: table-view twin for every chart (§4.2/§4.4) — both the WCAG relief channel for
// sub-3:1 fills and the "every chart has a table view" rule.
//
// review fix (r2): the original `<TableViewToggle>` wrapped BOTH the toggle-row buttons AND
// the chart/table content in one component, and callers nested it INSIDE ChartCard's
// fixed-height body — so the toggle row's own height ate into the chart's declared height,
// clipping/overflowing axes. Fix: split into a `useTableView()` state hook +
// `TableViewControls` (just the buttons, presentational). Callers now put the controls in
// ChartCard's `controls` header slot (rendered ABOVE the fixed-height body, never inside it)
// and switch their children between the chart and a `DataTable` based on `view`. This keeps
// the fixed-height box 100% chart (or 100% table), so nothing clips.
import { useState } from 'react';
import { DataTable } from '../components/DataTable';
import type { DataTableColumn } from '../components/DataTable';

export type TableViewMode = 'chart' | 'table';

/** State only — no rendering. Use with `TableViewControls` (header slot) and pick the
 *  chart vs. `DataTable` content yourself based on the returned `view`. */
export function useTableView(defaultView: TableViewMode = 'chart') {
  const [view, setView] = useState<TableViewMode>(defaultView);
  return { view, setView } as const;
}

interface TableViewControlsProps {
  view: TableViewMode;
  onChange: (view: TableViewMode) => void;
}

/** The chart|table pill toggle — presentational only. Render this in ChartCard's `controls`
 *  header slot, never inside the fixed-height chart body. */
export function TableViewControls({ view, onChange }: TableViewControlsProps) {
  return (
    <div style={{ display: 'inline-flex', gap: 2, background: 'var(--space-800)', borderRadius: 'var(--radius-sm)', padding: 2 }}>
      {(['chart', 'table'] as const).map(v => (
        <button
          key={v}
          type="button"
          onClick={() => onChange(v)}
          aria-pressed={view === v}
          style={{
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--fs-mono-sm)',
            textTransform: 'uppercase',
            letterSpacing: 'var(--ls-label)',
            padding: '3px 10px',
            borderRadius: 'var(--radius-xs)',
            border: 'none',
            cursor: 'pointer',
            background: view === v ? 'var(--grad-accent)' : 'transparent',
            color: view === v ? 'var(--accent-on)' : 'var(--text-muted)',
          }}
        >
          {v}
        </button>
      ))}
    </div>
  );
}

interface TableViewProps<T> {
  view: TableViewMode;
  columns: DataTableColumn<T>[];
  rows: T[];
  rowKey: (row: T, index: number) => string;
  children: React.ReactNode; // the chart, rendered when view === 'chart'
}

/** Renders the chart (children) when view === 'chart', else a DataTable of the same rows.
 *  Pure content switch — no toggle-row markup, so it's safe to use as ChartCard's entire
 *  fixed-height body (pair with `TableViewControls` in the `controls` header slot). */
export function TableView<T>({ view, columns, rows, rowKey, children }: TableViewProps<T>) {
  return view === 'chart' ? <>{children}</> : <DataTable columns={columns} rows={rows} rowKey={rowKey} />;
}
