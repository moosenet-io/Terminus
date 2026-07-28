// CONST-17: DataTable primitive per §2.3 — tracked-mono header, row hover, brand hairlines.
// Backed by the existing `.h-table` class in globals.css. Generic so both panels and the
// viz kit's TableViewToggle (src/viz/TableViewToggle.tsx) can render any row shape.
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
  /** LGUI-08: optional row-click handler (e.g. opening a detail Drawer). Additive/opt-in —
   *  every existing caller that doesn't pass it keeps rendering plain, non-interactive rows. */
  onRowClick?: (row: T) => void;
}

export function DataTable<T>({ columns, rows, rowKey, emptyMessage = 'No data', style, onRowClick }: DataTableProps<T>) {
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
        {rows.map((row, i) => (
          <tr
            key={rowKey(row, i)}
            onClick={onRowClick ? () => onRowClick(row) : undefined}
            style={onRowClick ? { cursor: 'pointer' } : undefined}
          >
            {columns.map(col => (
              <td key={col.key} style={{ textAlign: col.align ?? 'left', fontVariantNumeric: 'tabular-nums' }}>
                {col.render(row)}
              </td>
            ))}
          </tr>
        ))}
      </tbody>
    </table>
  );
}
