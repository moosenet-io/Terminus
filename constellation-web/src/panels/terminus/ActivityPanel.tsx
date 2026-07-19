// CONST-28: Terminus module self — activity view, built against the CONST-26 §8 contract
// (`GET /api/terminus/activity?limit=` -> `{entries:[{ts,method,path,principal,system}]}`).
// CONST-26 owns the Rust endpoint and lands in parallel — this panel does NOT implement it,
// only consumes the typed `client.terminus.activity()` method (aggregationClient.ts), which
// already degrades to `{available:false}` on 404/501/any failure (the house per-endpoint
// degrade pattern). Paged + filterable by system/method/principal.
import { useEffect, useMemo, useState } from 'react';
import { Card, CardTitle } from '../../components/Card';
import { Badge } from '../../components/Badge';
import { SkeletonList } from '../../components/Skeleton';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import { getAggregationClient } from '../../lib/aggregationClient';
import type { TerminusActivityEntry } from '../../lib/aggregationClient';

const PAGE_SIZE = 20;
const FETCH_LIMIT = 200;

type FilterKey = 'system' | 'method' | 'principal';

function distinctValues(entries: TerminusActivityEntry[], key: FilterKey): string[] {
  return Array.from(new Set(entries.map(e => e[key]))).sort();
}

export function ActivityPanel() {
  const [entries, setEntries] = useState<TerminusActivityEntry[]>([]);
  const [available, setAvailable] = useState<boolean | null>(null); // null = still loading
  const [detail, setDetail] = useState<string | undefined>(undefined);
  const [systemFilter, setSystemFilter] = useState<string | null>(null);
  const [methodFilter, setMethodFilter] = useState<string | null>(null);
  const [principalFilter, setPrincipalFilter] = useState<string | null>(null);
  const [page, setPage] = useState(0);

  useEffect(() => {
    let cancelled = false;
    getAggregationClient()
      .terminus.activity(FETCH_LIMIT)
      .then(res => {
        if (cancelled) return;
        setEntries(res.entries);
        setAvailable(res.available);
        setDetail(res.detail);
      })
      .catch(() => {
        // The typed method itself never throws (it catches internally), but guard anyway —
        // an unexpected throw still degrades gracefully rather than crashing the panel.
        if (!cancelled) { setAvailable(false); setEntries([]); }
      });
    return () => { cancelled = true; };
  }, []);

  const filtered = useMemo(() => {
    return entries.filter(e =>
      (!systemFilter || e.system === systemFilter) &&
      (!methodFilter || e.method === methodFilter) &&
      (!principalFilter || e.principal === principalFilter),
    );
  }, [entries, systemFilter, methodFilter, principalFilter]);

  const pageCount = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE));
  const clampedPage = Math.min(page, pageCount - 1);
  const pageRows = filtered.slice(clampedPage * PAGE_SIZE, clampedPage * PAGE_SIZE + PAGE_SIZE);

  const columns: DataTableColumn<TerminusActivityEntry>[] = [
    { key: 'ts', header: 'Time', render: r => new Date(r.ts).toLocaleTimeString() },
    { key: 'system', header: 'System', render: r => <Badge tone="violet">{r.system}</Badge> },
    { key: 'method', header: 'Method', render: r => <code style={{ fontFamily: 'var(--font-mono)' }}>{r.method}</code> },
    { key: 'path', header: 'Path', render: r => <code style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)' }}>{r.path}</code> },
    { key: 'principal', header: 'Principal', render: r => r.principal },
  ];

  function filterRow(label: string, key: FilterKey, value: string | null, setValue: (v: string | null) => void) {
    const options = distinctValues(entries, key);
    if (options.length === 0) return null;
    return (
      <div style={{ display: 'flex', gap: 'var(--space-1)', alignItems: 'center', flexWrap: 'wrap' }}>
        <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', marginRight: 4 }}>{label}:</span>
        <button
          type="button"
          onClick={() => { setValue(null); setPage(0); }}
          className={`h-badge ${value === null ? 'h-badge-violet' : 'h-badge-neutral'}`}
          style={{ cursor: 'pointer', border: 'none' }}
        >
          all
        </button>
        {options.map(o => (
          <button
            key={o}
            type="button"
            onClick={() => { setValue(o); setPage(0); }}
            className={`h-badge ${value === o ? 'h-badge-violet' : 'h-badge-neutral'}`}
            style={{ cursor: 'pointer', border: 'none' }}
          >
            {o}
          </button>
        ))}
      </div>
    );
  }

  return (
    <div style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="Recent cross-system requests, filterable by system/method/principal">
        Terminus — Activity
      </CardTitle>

      {available === null && (
        <Card variant="content">
          <SkeletonList rows={6} />
        </Card>
      )}

      {available === false && (
        <Card variant="content">
          <div style={{ textAlign: 'center', padding: 'var(--space-4)', color: 'var(--text-muted)' }}>
            <div style={{ fontSize: 'var(--fs-sm)' }}>Activity feed isn't live on this backend yet.</div>
            <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-faint)', marginTop: 'var(--space-1)' }}>
              {detail ?? 'GET /api/terminus/activity is not reachable (404/501) — this lands with CONST-26.'}
            </div>
          </div>
        </Card>
      )}

      {available === true && (
        <>
          <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)' }}>
            {filterRow('System', 'system', systemFilter, setSystemFilter)}
            {filterRow('Method', 'method', methodFilter, setMethodFilter)}
            {filterRow('Principal', 'principal', principalFilter, setPrincipalFilter)}
          </div>

          <Card variant="content">
            <DataTable
              columns={columns}
              rows={pageRows}
              rowKey={(r, i) => `${r.ts}-${r.path}-${i}`}
              emptyMessage={entries.length === 0 ? 'No activity recorded' : 'No activity matches this filter'}
            />
          </Card>

          {filtered.length > 0 && (
            <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', fontSize: 'var(--fs-xs)', color: 'var(--text-muted)' }}>
              <button
                type="button"
                disabled={clampedPage === 0}
                onClick={() => setPage(p => Math.max(0, p - 1))}
                style={{ opacity: clampedPage === 0 ? 0.4 : 1 }}
              >
                ← prev
              </button>
              <span>
                page {clampedPage + 1} / {pageCount} · {filtered.length} entr{filtered.length === 1 ? 'y' : 'ies'}
              </span>
              <button
                type="button"
                disabled={clampedPage >= pageCount - 1}
                onClick={() => setPage(p => Math.min(pageCount - 1, p + 1))}
                style={{ opacity: clampedPage >= pageCount - 1 ? 0.4 : 1 }}
              >
                next →
              </button>
            </div>
          )}
        </>
      )}
    </div>
  );
}
