// CONST-24 §7.3: C9 (trade-off parallel coordinates), composed into the Capability section
// (§7.3 "Capability = C1+C9") — this file owns only the C9 ChartCard; CapabilitySection.tsx
// renders it directly after C1 inside the same `<section id="capability">` so the spec's
// section composition holds even though the two charts live in separate files (mirrors the
// CONST-23 `mintShared.ts` convention of small shared pieces rather than one giant file).
import { useMemo } from 'react';
import { ChartCard } from '../../viz/ChartCard';
import { ParallelCoordinatesChart, partitionCompleteTradeoffs } from '../../viz/ParallelCoordinatesChart';
import { SlotAssigner } from '../../viz/palette';
import { TableView, TableViewControls, useTableView } from '../../viz/TableViewToggle';
import type { DataTableColumn } from '../../components/DataTable';
import { useMintTradeoffs } from '../../hooks/useMint';
import type { MintFilters } from '../../hooks/useMint';
import type { MintTradeoffPoint } from '../../lib/aggregationClient';

const CHART_HEIGHT = 320;

interface TableRow {
  model: string;
  mean_score: string;
  pass_hat_3: string;
  mean_throughput: string;
  p95_latency_ms: string;
  vram_gb: string;
  max_context_safe: string;
}

export function TradeoffsSection({ filters }: { filters: MintFilters }) {
  const tradeoffs = useMintTradeoffs(filters);
  const { view, setView } = useTableView();
  const slots = useMemo(() => new SlotAssigner(), []);

  const dims = tradeoffs.data?.dims ?? [];
  const { complete, excludedCount } = useMemo(
    () => partitionCompleteTradeoffs(dims, tradeoffs.data?.points ?? []),
    [dims, tradeoffs.data],
  );

  const selectedModels = filters.models;

  const columns: DataTableColumn<TableRow>[] = [
    { key: 'model', header: 'Model', render: r => r.model },
    { key: 'score', header: 'Mean score', align: 'right', render: r => r.mean_score },
    { key: 'pass3', header: 'pass^3', align: 'right', render: r => r.pass_hat_3 },
    { key: 'throughput', header: 'Throughput', align: 'right', render: r => r.mean_throughput },
    { key: 'p95', header: 'p95 latency', align: 'right', render: r => r.p95_latency_ms },
    { key: 'vram', header: 'VRAM', align: 'right', render: r => r.vram_gb },
    { key: 'ctx', header: 'Max safe context', align: 'right', render: r => r.max_context_safe },
  ];

  const tableRows: TableRow[] = complete.map((p: MintTradeoffPoint) => ({
    model: p.model,
    mean_score: p.raw.mean_score != null ? String(p.raw.mean_score) : '—',
    pass_hat_3: p.raw.pass_hat_3 != null ? String(p.raw.pass_hat_3) : '—',
    mean_throughput: p.raw.mean_throughput != null ? String(p.raw.mean_throughput) : '—',
    p95_latency_ms: p.raw.p95_latency_ms != null ? String(p.raw.p95_latency_ms) : '—',
    vram_gb: p.raw.vram_gb != null ? String(p.raw.vram_gb) : '—',
    max_context_safe: p.raw.max_context_safe != null ? String(p.raw.max_context_safe) : '—',
  }));

  const empty = !tradeoffs.loading && complete.length < 2;

  return (
    <div style={{ marginTop: 'var(--space-3)' }}>
      <ChartCard
        title="Trade-off parallel coordinates"
        subtitle="mean_score, pass^3, throughput, p95 latency (inv), VRAM (inv), max safe context · drag an axis to brush-filter"
        height={CHART_HEIGHT}
        loading={tradeoffs.loading && !tradeoffs.data}
        isRefetching={tradeoffs.loading && !!tradeoffs.data}
        degraded={tradeoffs.degraded}
        empty={empty}
        emptyMessage="Fewer than 2 models have all 6 trade-off dimensions profiled"
        controls={<TableViewControls view={view} onChange={setView} />}
        footer={excludedCount > 0 ? (
          <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-faint)' }}>
            {excludedCount} model{excludedCount === 1 ? '' : 's'} excluded — missing at least one of the 6 dimensions
          </div>
        ) : undefined}
      >
        <TableView view={view} columns={columns} rows={tableRows} rowKey={(r, i) => `${r.model}-${i}`}>
          <ParallelCoordinatesChart
            dims={dims}
            points={complete}
            selectedModels={selectedModels}
            colorFor={model => slots.colorFor(model)}
            height={CHART_HEIGHT - 40}
          />
        </TableView>
      </ChartCard>
    </div>
  );
}
