// CONST-23 §7.3: Capability = C1 (capability radar) + a reserved C9 slot ("phase 2" —
// parallel-coordinates lands in CONST-24; the section is structured so it can slot in without
// a layout change).
import { useMemo } from 'react';
import { ChartCard } from '../../viz/ChartCard';
import { RadarChart } from '../../viz/RadarChart';
import type { RadarDatum, RadarSeries, RadarVertexMeta } from '../../viz/RadarChart';
import { SlotAssigner, CHART_CHROME } from '../../viz/palette';
import { ChartLegend } from '../../viz/ChartLegend';
import { TableView, TableViewControls, useTableView } from '../../viz/TableViewToggle';
import type { DataTableColumn } from '../../components/DataTable';
import { useMintDimensions } from '../../hooks/useMint';
import type { MintFilters } from '../../hooks/useMint';
import { mintSectionTitleStyle } from './mintShared';

const CHART_HEIGHT = 380;
const FLEET_MEDIAN_ID = 'fleet_median';

interface TableRow {
  model: string;
  dimension: string;
  norm: number;
  raw: number;
  std_dev: number;
  n: number;
  low_confidence: boolean;
  missing: boolean;
}

export function CapabilitySection({ filters }: { filters: MintFilters }) {
  const dimensions = useMintDimensions(filters);
  const { view, setView } = useTableView();
  const slots = useMemo(() => new SlotAssigner(), []);

  const displayedModels = useMemo(() => {
    if (!dimensions.data) return [];
    if (filters.models.length === 0) return [];
    return dimensions.data.models.filter(m => filters.models.includes(m.model_id));
  }, [dimensions.data, filters.models]);

  const zeroRunsThisEpoch = !dimensions.loading && !!dimensions.data && dimensions.data.models.length === 0;
  const needsSelection = !dimensions.loading && !zeroRunsThisEpoch && displayedModels.length < 2;

  const series: RadarSeries[] = useMemo(() => {
    const out: RadarSeries[] = displayedModels.map(m => ({ id: m.model_id, label: m.model_id, color: slots.colorFor(m.model_id) }));
    out.push({ id: FLEET_MEDIAN_ID, label: 'fleet median', color: CHART_CHROME.deemphasis, isReference: true });
    return out;
  }, [displayedModels, slots]);

  const { radarData, meta, tableRows } = useMemo(() => {
    const dims = dimensions.data?.dimensions ?? [];
    const meta = new Map<string, RadarVertexMeta>();
    const tableRows: TableRow[] = [];
    const radarData: RadarDatum[] = dims.map(dimension => {
      const row: RadarDatum = { dimension };
      for (const m of displayedModels) {
        const score = m.scores.find(s => s.dimension === dimension);
        const missing = !score;
        row[m.model_id] = score?.norm ?? 0;
        meta.set(`${dimension}::${m.model_id}`, {
          raw: score?.raw ?? 0, std_dev: score?.std_dev ?? 0, n: score?.n ?? 0,
          low_confidence: score?.low_confidence ?? false, missing,
        });
        tableRows.push({
          model: m.model_id, dimension, norm: score?.norm ?? 0, raw: score?.raw ?? 0,
          std_dev: score?.std_dev ?? 0, n: score?.n ?? 0,
          low_confidence: score?.low_confidence ?? false, missing,
        });
      }
      const median = dimensions.data?.fleet_median.find(s => s.dimension === dimension);
      row[FLEET_MEDIAN_ID] = median?.norm ?? 0;
      meta.set(`${dimension}::${FLEET_MEDIAN_ID}`, {
        raw: median?.raw ?? 0, std_dev: median?.std_dev ?? 0, n: median?.n ?? 0,
        low_confidence: median?.low_confidence ?? false, missing: !median,
      });
      return row;
    });
    return { radarData, meta, tableRows };
  }, [dimensions.data, displayedModels]);

  const columns: DataTableColumn<TableRow>[] = [
    { key: 'model', header: 'Model', render: r => r.model },
    { key: 'dimension', header: 'Dimension', render: r => r.dimension },
    { key: 'norm', header: 'Norm', align: 'right', render: r => r.norm.toFixed(2) },
    { key: 'raw', header: 'Raw', align: 'right', render: r => r.raw.toFixed(2) },
    { key: 'std_dev', header: '±stddev', align: 'right', render: r => r.std_dev.toFixed(2) },
    { key: 'n', header: 'n', align: 'right', render: r => String(r.n) },
    { key: 'flags', header: 'Flags', render: r => [r.low_confidence && '⚠ low n', r.missing && '⚠ not profiled'].filter(Boolean).join(' ') || '—' },
  ];

  return (
    <section id="capability" style={{ scrollMarginTop: 64 }}>
      <h3 style={mintSectionTitleStyle}>Capability</h3>
      <ChartCard
        title="Capability radar"
        subtitle="8 assistant dimensions · up to 4 models + fleet median"
        height={CHART_HEIGHT}
        loading={dimensions.loading && !dimensions.data}
        isRefetching={dimensions.loading && !!dimensions.data}
        degraded={dimensions.degraded}
        empty={needsSelection || zeroRunsThisEpoch}
        emptyMessage={zeroRunsThisEpoch ? 'No assistant-dimension runs recorded for this epoch' : 'Select at least 2 models in the filter row to compare capability'}
        emptyHint={zeroRunsThisEpoch ? 'try epoch=all or a different epoch' : undefined}
        controls={<TableViewControls view={view} onChange={setView} />}
        footer={<ChartLegend entries={series.map(s => ({ id: s.id, label: s.label, color: s.color }))} />}
      >
        <TableView view={view} columns={columns} rows={tableRows} rowKey={(r, i) => `${r.model}-${r.dimension}-${i}`}>
          <RadarChart data={radarData} series={series} height={CHART_HEIGHT - 60} meta={meta} />
        </TableView>
      </ChartCard>

      {/* C9 (parallel coordinates) — CONST-24 slot. Reserved so the 2-col grid doesn't need to
          change shape when it lands. */}
      <div style={{ marginTop: 'var(--space-3)' }}>
        <ChartCard title="Trade-off parallel coordinates" subtitle="phase 2 — CONST-24" height={120} empty emptyMessage="Coming in CONST-24" emptyHint="6-dim normalized trade-off view (score, pass_hat_3, throughput, latency, vram, max context)">
          <div />
        </ChartCard>
      </div>
    </section>
  );
}
