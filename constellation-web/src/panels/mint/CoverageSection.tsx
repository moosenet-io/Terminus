// CONST-23 §7.3: Coverage = C2 (score/coverage heatmap), full-width per §7.3's grid rule.
import { useMemo, useState } from 'react';
import { ChartCard } from '../../viz/ChartCard';
import { HeatmapChart } from '../../viz/HeatmapChart';
import type { HeatmapRow, HeatmapDatum } from '../../viz/HeatmapChart';
import { SEQUENTIAL_HEX } from '../../viz/palette';
import { TableView, TableViewControls, useTableView } from '../../viz/TableViewToggle';
import type { DataTableColumn } from '../../components/DataTable';
import { useMintMatrix } from '../../hooks/useMint';
import type { MintFilters } from '../../hooks/useMint';
import type { MintMatrixCell } from '../../lib/aggregationClient';
import { mintSectionTitleStyle } from './mintShared';

const CHART_HEIGHT = 420;

function statusGlyph(status: MintMatrixCell['status']): string {
  return status === 'not_run' ? '—' : status === 'stale' ? '🕐' : status === 'non_viable' ? '✕' : '';
}

export function CoverageSection({ filters }: { filters: MintFilters }) {
  const matrix = useMintMatrix(filters);
  const { view, setView } = useTableView();

  const rows: HeatmapRow[] = useMemo(() => {
    if (!matrix.data) return [];
    const byModel = new Map<string, HeatmapDatum[]>();
    for (const model of matrix.data.models) byModel.set(model, []);
    for (const cell of matrix.data.cells) {
      const arr = byModel.get(cell.model);
      if (!arr) continue;
      arr.push({
        x: cell.col,
        y: cell.pass_rate,
        status: cell.status,
        n_samples: cell.n_samples,
        score_stddev: cell.score_stddev,
        low_confidence: cell.low_confidence,
        last_run_at: cell.last_run_at,
        harness_version: cell.harness_version,
      });
    }
    // Rows sorted by mean pass_rate desc (§7.2), 'ok' cells only feed the mean.
    return [...byModel.entries()]
      .map(([id, data]) => {
        const ok = data.filter(d => d.status === 'ok' && d.y != null);
        const mean = ok.length ? ok.reduce((a, d) => a + (d.y ?? 0), 0) / ok.length : -1;
        return { id, data, mean };
      })
      .sort((a, b) => b.mean - a.mean)
      .map(({ id, data }) => ({ id, data }));
  }, [matrix.data]);

  const tableRows = useMemo(() => {
    if (!matrix.data) return [];
    return matrix.data.cells;
  }, [matrix.data]);

  const columns: DataTableColumn<MintMatrixCell>[] = [
    { key: 'model', header: 'Model', render: r => r.model },
    { key: 'col', header: 'Column', render: r => r.col },
    { key: 'status', header: 'Status', render: r => `${statusGlyph(r.status)} ${r.status}`.trim() },
    { key: 'pass_rate', header: 'Pass rate', align: 'right', render: r => r.pass_rate != null ? `${Math.round(r.pass_rate * 100)}%` : '—' },
    { key: 'n', header: 'n', align: 'right', render: r => String(r.n_samples) },
  ];

  const corpusUnset = matrix.data?.corpus_dir_unset ?? false;

  return (
    <section id="coverage" style={{ scrollMarginTop: 64 }}>
      <h3 style={mintSectionTitleStyle}>Coverage</h3>
      <ChartCard
        title="Score / coverage matrix"
        subtitle="rows = models (by mean pass_rate) · cols = test_type × task_category"
        height={CHART_HEIGHT}
        loading={matrix.loading && !matrix.data}
        isRefetching={matrix.loading && !!matrix.data}
        degraded={matrix.degraded}
        empty={!matrix.loading && rows.length === 0}
        emptyMessage="No coverage data for this filter"
        controls={<TableViewControls view={view} onChange={setView} />}
        footer={
          <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
            <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)' }}>
              <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)' }}>low</span>
              <div style={{ display: 'flex' }}>
                {SEQUENTIAL_HEX.map(hex => (
                  <span key={hex} style={{ width: 18, height: 8, background: hex }} />
                ))}
              </div>
              <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)' }}>high (pass_rate)</span>
              <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-faint)', marginLeft: 12 }}>
                — not run · 🕐 stale (55% opacity) · ✕ non-viable
              </span>
            </div>
            {corpusUnset && (
              <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-faint)' }}>
                code/agent columns show all "—" (not_run) because INTAKE_CORPUS_V2_DIR is not yet
                provisioned on this host — this is a known operator prerequisite, not a data gap.
              </div>
            )}
          </div>
        }
      >
        <TableView view={view} columns={columns} rows={tableRows} rowKey={(r, i) => `${r.model}-${r.col}-${i}`}>
          <HeatmapChart data={rows} height={CHART_HEIGHT - 40} />
        </TableView>
      </ChartCard>
    </section>
  );
}
