// S127 TGUI2 POL-06/08: the dense, professional data-table primitive — the density template the
// design direction calls for ("data tables, not sparse cards, for list data"; Cloudflare/Linear
// row density). Distinct from the simpler read-only `DataTable`: this one adds
//   • click-to-sort columns with a caret indicator (client-side, stable),
//   • an optional expand-for-detail affordance (a compact chevron opens a full-width detail row),
//   • an optional pagination footer when the row set exceeds a page size.
// Backed by the `.h-dtable*` classes in globals.css (all geometry lives in CSS — token-only, no
// raw px/hex in this file). Generic over the row type so Tools and Roster share one implementation.
import { Fragment, useMemo, useState } from 'react';

export interface SortableColumn<T> {
  key: string;
  header: string;
  align?: 'left' | 'right' | 'center';
  /** Fixed column width (any CSS length, e.g. '30%' or '9rem'). */
  width?: string;
  /** When true the header is a sort trigger; requires `sortValue`. */
  sortable?: boolean;
  /** The comparable value for sorting this column (string → locale compare, number → numeric). */
  sortValue?: (row: T) => string | number;
  render: (row: T) => React.ReactNode;
}

interface SortableTableProps<T> {
  columns: SortableColumn<T>[];
  rows: T[];
  rowKey: (row: T, index: number) => string;
  /** Initial sort — omit for the natural (unsorted) row order. */
  initialSort?: { key: string; dir: 'asc' | 'desc' };
  /** When set and rows exceed it, a pagination footer appears and only a page is rendered. */
  pageSize?: number;
  /** Optional per-row detail; returning a node makes the row expandable via a leading chevron. */
  expandable?: (row: T) => React.ReactNode | null;
  emptyMessage?: string;
}

type SortState = { key: string; dir: 'asc' | 'desc' } | null;

export function SortableTable<T>({
  columns,
  rows,
  rowKey,
  initialSort,
  pageSize,
  expandable,
  emptyMessage = 'No data',
}: SortableTableProps<T>) {
  const [sort, setSort] = useState<SortState>(initialSort ?? null);
  const [page, setPage] = useState(0);
  const [openKey, setOpenKey] = useState<string | null>(null);

  const sorted = useMemo(() => {
    if (!sort) return rows;
    const col = columns.find(c => c.key === sort.key);
    if (!col?.sortValue) return rows;
    const dir = sort.dir === 'asc' ? 1 : -1;
    // stable sort on a shallow copy so the source order is preserved for equal keys
    return [...rows].sort((a, b) => {
      const av = col.sortValue!(a);
      const bv = col.sortValue!(b);
      if (typeof av === 'number' && typeof bv === 'number') return (av - bv) * dir;
      return String(av).localeCompare(String(bv)) * dir;
    });
  }, [rows, sort, columns]);

  const total = sorted.length;
  const pageCount = pageSize ? Math.max(1, Math.ceil(total / pageSize)) : 1;
  // clamp the page if the row set shrank (e.g. a filter narrowed it under the current offset)
  const safePage = Math.min(page, pageCount - 1);
  const view = pageSize && total > pageSize ? sorted.slice(safePage * pageSize, safePage * pageSize + pageSize) : sorted;

  const toggleSort = (key: string) => {
    setPage(0);
    setSort(prev => {
      if (prev?.key === key) return { key, dir: prev.dir === 'asc' ? 'desc' : 'asc' };
      return { key, dir: 'asc' };
    });
  };

  const colSpan = columns.length + (expandable ? 1 : 0);

  if (total === 0) {
    return (
      <div style={{ padding: 'var(--space-5)', textAlign: 'center', color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>
        {emptyMessage}
      </div>
    );
  }

  return (
    <div>
      <div style={{ overflowX: 'auto' }}>
        <table className="h-dtable">
          <thead>
            <tr>
              {expandable && <th style={{ width: '1%' }} aria-hidden />}
              {columns.map(col => {
                const active = sort?.key === col.key;
                return (
                  <th key={col.key} style={{ textAlign: col.align ?? 'left', width: col.width }}>
                    {col.sortable && col.sortValue ? (
                      <button
                        type="button"
                        className="h-dtable-sortbtn"
                        onClick={() => toggleSort(col.key)}
                        aria-label={`Sort by ${col.header}`}
                        style={{ justifyContent: col.align === 'right' ? 'flex-end' : 'flex-start' }}
                      >
                        {col.header}
                        <span className="h-dtable-caret" data-active={active} aria-hidden>
                          {active ? (sort!.dir === 'asc' ? '▲' : '▼') : '⇅'}
                        </span>
                      </button>
                    ) : (
                      col.header
                    )}
                  </th>
                );
              })}
            </tr>
          </thead>
          <tbody>
            {view.map((row, i) => {
              const key = rowKey(row, i);
              const detail = expandable ? expandable(row) : null;
              const isOpen = openKey === key;
              const toggle = detail ? () => setOpenKey(prev => (prev === key ? null : key)) : undefined;
              return (
                <Fragment key={key}>
                  <tr
                    className={detail ? 'h-dtable-row h-dtable-row-expandable' : 'h-dtable-row'}
                    {...(toggle ? { onClick: toggle, role: 'button', tabIndex: 0, onKeyDown: (e: React.KeyboardEvent) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); toggle(); } } } : {})}
                  >
                    {expandable && (
                      <td style={{ width: '1%' }}>
                        {detail && <span className="h-dtable-chevron" data-open={isOpen} aria-hidden>▶</span>}
                      </td>
                    )}
                    {columns.map(col => (
                      <td key={col.key} style={{ textAlign: col.align ?? 'left', ...(col.align === 'right' ? { fontVariantNumeric: 'tabular-nums' } : {}) }}>
                        {col.render(row)}
                      </td>
                    ))}
                  </tr>
                  {detail && isOpen && (
                    <tr>
                      <td className="h-dtable-detail-td" colSpan={colSpan}>
                        <div className="h-dtable-detail-inner">{detail}</div>
                      </td>
                    </tr>
                  )}
                </Fragment>
              );
            })}
          </tbody>
        </table>
      </div>

      {pageSize && total > pageSize && (
        <div className="h-dtable-foot">
          <span className="h-dtable-count">
            {safePage * pageSize + 1}–{Math.min((safePage + 1) * pageSize, total)} of {total}
          </span>
          <div style={{ display: 'inline-flex', gap: 'var(--space-1)' }}>
            <button type="button" className="h-pagebtn" onClick={() => setPage(p => Math.max(0, p - 1))} disabled={safePage === 0}>Prev</button>
            <span className="h-dtable-count" style={{ paddingBlock: 3, paddingInline: 6 }}>{safePage + 1} / {pageCount}</span>
            <button type="button" className="h-pagebtn" onClick={() => setPage(p => Math.min(pageCount - 1, p + 1))} disabled={safePage >= pageCount - 1}>Next</button>
          </div>
        </div>
      )}
    </div>
  );
}
