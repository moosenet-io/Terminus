// CONST-17: ChartCard — the mandatory wrapper for every chart in every module (§4.3). Card
// (content variant) + header row (title, optional subtitle, right-aligned controls slot) +
// body (chart, height fixed so the container never clips axis labels into a nested scroll)
// + footer slot (table toggle, caveats). Loading/refetch/empty states per §2.6/§4.3.
// Filters NEVER live inside a ChartCard (dataviz rule) — pass them above, in the section header.
import { Card } from '../components/Card';
import { ChartSkeleton } from './ChartSkeleton';
import { ChartEmpty } from './ChartEmpty';

interface ChartCardProps {
  title: string;
  subtitle?: string;
  /** Right-aligned controls in the header row (e.g. a log-scale toggle). */
  controls?: React.ReactNode;
  /** Body height in px — includes the x-axis band. */
  height: number;
  loading?: boolean;
  /** Previous render, kept at 0.6 opacity during a refetch instead of re-skeletoning (§2.6). */
  isRefetching?: boolean;
  empty?: boolean;
  emptyMessage?: string;
  emptyHint?: string;
  /** Degraded backend ({available:false, detail}) — render the module-standard degraded
   *  card instead of chart content. */
  degraded?: { detail?: string } | false;
  /** Footer slot: a ChartLegend and/or caveats live here. The table/chart toggle buttons go
   *  in `controls` (above), not here — see viz/TableViewToggle.tsx. */
  footer?: React.ReactNode;
  children: React.ReactNode;
}

export function ChartCard({
  title,
  subtitle,
  controls,
  height,
  loading = false,
  isRefetching = false,
  empty = false,
  emptyMessage = 'No data for this filter',
  emptyHint,
  degraded = false,
  footer,
  children,
}: ChartCardProps) {
  return (
    <Card variant="content">
      <div style={{ display: 'flex', alignItems: 'baseline', justifyContent: 'space-between', gap: 'var(--space-2)', marginBottom: 'var(--space-2)' }}>
        <div>
          <div style={{ fontSize: 13, fontWeight: 600, color: 'var(--text-100)' }}>{title}</div>
          {subtitle && <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', marginTop: 2 }}>{subtitle}</div>}
        </div>
        {controls && <div>{controls}</div>}
      </div>

      {/* review fix (r2): this box is ALWAYS 100% chart (or 100% table) — the table-view
          toggle row lives in the `controls` header slot above, never in here, so it can
          never eat into the declared chart height and clip the axis band. `overflowY:auto`
          is a safety net for the table view when a slice has more rows than the chart's
          height accommodates (never clipped silently by the Card's own overflow:hidden). */}
      <div style={{ height, opacity: isRefetching ? 0.6 : 1, transition: 'opacity var(--dur-base) var(--ease-out)', overflowY: 'auto' }}>
        {degraded ? (
          <ChartEmpty height={height} message="Module unavailable" hint={degraded.detail ?? 'backend not reachable'} />
        ) : loading ? (
          <ChartSkeleton height={height} />
        ) : empty ? (
          <ChartEmpty height={height} message={emptyMessage} hint={emptyHint} />
        ) : (
          children
        )}
      </div>

      {footer && !loading && !degraded && (
        <div style={{ marginTop: 'var(--space-2)' }}>{footer}</div>
      )}
    </Card>
  );
}
