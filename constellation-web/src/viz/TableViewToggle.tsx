// CONST-17: TableViewToggle — swaps the plot for a DataTable of the same slice (§4.2/§4.4).
// Mandatory on every chart: it's both the accessibility floor (WCAG relief channel for
// sub-3:1 fills) and the "every chart has a table view" rule.
import { useState } from 'react';
import { DataTable } from '../components/DataTable';
import type { DataTableColumn } from '../components/DataTable';

interface TableViewToggleProps<T> {
  columns: DataTableColumn<T>[];
  rows: T[];
  rowKey: (row: T, index: number) => string;
  children: React.ReactNode; // the chart, rendered when the toggle is in "chart" mode
  defaultView?: 'chart' | 'table';
}

export function TableViewToggle<T>({ columns, rows, rowKey, children, defaultView = 'chart' }: TableViewToggleProps<T>) {
  const [view, setView] = useState<'chart' | 'table'>(defaultView);
  return (
    <div>
      <div style={{ display: 'flex', justifyContent: 'flex-end', marginBottom: 'var(--space-1)' }}>
        <div style={{ display: 'inline-flex', gap: 2, background: 'var(--space-800)', borderRadius: 'var(--radius-sm)', padding: 2 }}>
          {(['chart', 'table'] as const).map(v => (
            <button
              key={v}
              type="button"
              onClick={() => setView(v)}
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
      </div>
      {view === 'chart' ? children : (
        <DataTable columns={columns} rows={rows} rowKey={rowKey} />
      )}
    </div>
  );
}
