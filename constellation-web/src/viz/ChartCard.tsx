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
  /**
   * Body height, including the x-axis band.
   *
   * MGUI-18 WIDENED this from `number` to `number | string`, deliberately as a WIDENING
   * rather than a redefinition: `ChartCard` is the mandatory wrapper for every chart in
   * every module (MINT / Lumina / Chord / Harmony / Muse), and a plain number still means
   * exactly what it always did — px, via React's own style semantics. Every existing
   * `height={560}` caller is byte-for-byte unaffected.
   *
   * A string may be any CSS length, which is how a panel opts into a FLUID body that
   * follows the viewport: `height={fluidBodyHeight({min, max, reserve})}` (lib/catalogLayout)
   * emits a `clamp(min, calc(100dvh - reserve), max)`. The clamp is what makes it responsive
   * with no resize listener — the browser re-resolves it on every viewport change, so it can
   * never report a stale size the way a JS-measured height can. A fixed px height stays the
   * right answer for a small stat/sparkline card where fluid height would just add slack.
   */
  height: number | string;
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
      {/* MGUI-18: `flexWrap` + `minWidth: 0` on the text block. Without them a long subtitle
          (the Library's "240 of 240 loaded · 1892 in library · 1629 on disk") plus a controls
          slot pushes this row wider than the card on a narrow viewport — the header, not the
          chart, becomes what forces a sideways scroll. Wrapping moves the controls onto their
          own line instead. Applies to every module's cards, which all had the same latent
          overflow; a card with no controls and a short subtitle never wraps, so nothing that
          fits today changes. */}
      <div style={{ display: 'flex', alignItems: 'baseline', justifyContent: 'space-between', gap: 'var(--space-2)', marginBottom: 'var(--space-2)', flexWrap: 'wrap' }}>
        <div style={{ minWidth: 0 }}>
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
