// CONST-17: DataTable primitive per §2.3 — tracked-mono header, row hover, brand hairlines.
// Backed by the existing `.h-table` class in globals.css. Generic so both panels and the
// viz kit's TableViewToggle (src/viz/TableViewToggle.tsx) can render any row shape.
// CONST-24: added an optional `highlightRowKey` — the run-drill-down click-through target for
// C5 (§7.2 "swarm dot -> run row in table view") needs to land on and visually mark a specific
// row when a chart switches this table into view, without every other caller having to know
// about it.
export interface DataTableColumn<T> {
  key: string;
  header: string;
  align?: 'left' | 'right' | 'center';
  render: (row: T) => React.ReactNode;
}

interface DataTableProps<T> {
  columns: DataTableColumn<T>[];
  rows: T[];
  rowKey: (row: T, index: number) => string;
  emptyMessage?: string;
  style?: React.CSSProperties;
  /** When set and matching a row's `rowKey`, that row gets the accent-highlighted treatment
   *  and is scrolled into view on mount. */
  highlightRowKey?: string;
}

export function DataTable<T>({ columns, rows, rowKey, emptyMessage = 'No data', style, highlightRowKey }: DataTableProps<T>) {
  if (rows.length === 0) {
    return (
      <div style={{ padding: 'var(--space-5)', textAlign: 'center', color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>
        {emptyMessage}
      </div>
    );
  }
  return (
    <table className="h-table" style={style}>
      <thead>
        <tr>
          {columns.map(col => (
            <th key={col.key} style={{ textAlign: col.align ?? 'left' }}>{col.header}</th>
          ))}
        </tr>
      </thead>
      <tbody>
        {rows.map((row, i) => {
          const key = rowKey(row, i);
          const highlighted = highlightRowKey != null && key === highlightRowKey;
          return (
            <tr
              key={key}
              ref={highlighted ? (el => el?.scrollIntoView({ block: 'center' })) : undefined}
              style={highlighted ? { background: 'var(--bg-elevated)', boxShadow: 'inset 2px 0 0 var(--accent-bright)' } : undefined}
            >
              {columns.map(col => (
                <td key={col.key} style={{ textAlign: col.align ?? 'left', fontVariantNumeric: 'tabular-nums' }}>
                  {col.render(row)}
                </td>
              ))}
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}
