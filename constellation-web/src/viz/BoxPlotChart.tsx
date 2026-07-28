// CGUI-10 (TERM #533): bespoke box-and-whisker distribution chart, hand-drawn in SVG.
//
// Why not @nivo/boxplot: nivo's boxplot recomputes quantiles from RAW sample points, but the
// MINT box endpoints (`mint.box` / `mint.category/*/box`) return a PRE-COMPUTED 5-number
// summary (min/q1/median/q3/max) plus explicit outliers. Synthesizing fake raw samples to feed
// nivo would misrepresent the data; a direct summary renderer is both honest and fully
// brand-controllable. Horizontal layout: one row per model, shared value axis, median tick,
// IQR box, whiskers to min/max, outlier dots. Colors are stable categorical slots (color
// follows the model, not its rank, §4.2). Tokens only; SVG coords are unitless numbers.
//
// CGUI-10/CONST-23/24 reconciliation (low-n beeswarm): ported from CONST-24's BoxPlotChart,
// whose n<5 groups rendered a jittered dot strip instead of a box (a 5-number "IQR" computed
// from fewer than 5 samples misrepresents a spread that doesn't really exist yet). CONST-24's
// version needed raw per-run sample values for that strip, which the real `mint.box` /
// `mint.category/*/box` responses don't carry — so this port uses the fields the real endpoint
// DOES return for a low-n group: the five summary points (min/q1/median/q3/max, deduped) plus
// any recorded outliers, jittered as individual dots on the shared axis. Still fewer than 5
// points for n<5, but every dot is a real observed value, never a synthesized quartile box.
import { useLayoutEffect, useRef, useState } from 'react';
import { CATEGORICAL_HEX, CHART_CHROME } from './palette';
import type { BoxVM, BoxGroupVM } from '../panels/mint/transforms';

interface BoxPlotChartProps {
  vm: BoxVM;
  /** Formats an axis / tooltip value in the metric's native units. */
  formatValue: (v: number) => string;
  height: number;
}

/** Tracks the rendered width of a wrapper so the SVG can lay out to real pixels (a viewBox
 *  scale would distort text). Falls back to 600 before the first measure. */
function useElementWidth() {
  const ref = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(600);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const ro = new ResizeObserver(entries => {
      const w = entries[0]?.contentRect.width;
      if (w && w > 0) setWidth(w);
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  return { ref, width } as const;
}

const ROW_H = 34;
const LABEL_W = 132;
const PAD_R = 20;
const AXIS_H = 22;
const BOX_H = 16;

export function BoxPlotChart({ vm, formatValue, height }: BoxPlotChartProps) {
  const { ref, width } = useElementWidth();
  const groups = vm.groups;

  const domain = computeDomain(groups);
  const plotLeft = LABEL_W;
  const plotRight = Math.max(width - PAD_R, plotLeft + 40);
  const plotW = plotRight - plotLeft;
  const scale = (v: number) => {
    if (domain.max === domain.min) return plotLeft + plotW / 2;
    return plotLeft + ((v - domain.min) / (domain.max - domain.min)) * plotW;
  };

  const chartH = groups.length * ROW_H + AXIS_H;
  const ticks = axisTicks(domain.min, domain.max, 4);

  return (
    <div ref={ref} style={{ width: '100%', height, overflowY: 'auto' }}>
      <svg width={width} height={Math.max(chartH, height - 4)} role="img" aria-label="Distribution box plot">
        {/* axis gridlines + tick labels */}
        {ticks.map(t => {
          const x = scale(t);
          return (
            <g key={`t-${t}`}>
              <line x1={x} y1={AXIS_H} x2={x} y2={chartH} stroke="var(--chart-grid)" strokeWidth={1} />
              <text x={x} y={14} textAnchor="middle" fontSize={11} fill="var(--text-muted)" fontFamily="var(--font-mono)">
                {formatValue(t)}
              </text>
            </g>
          );
        })}

        {groups.map((g, i) => {
          const cy = AXIS_H + i * ROW_H + ROW_H / 2;
          const color = i < CATEGORICAL_HEX.length ? CATEGORICAL_HEX[i] : CHART_CHROME.deemphasis;

          if (g.lowN) {
            // n<5: a 5-number "IQR" computed from fewer than 5 samples misrepresents a spread
            // that doesn't really exist yet — render every real observed value (the summary
            // points, deduped, plus any outliers) as individual jittered dots instead of a box.
            const rawPoints = Array.from(new Set([g.min, g.q1, g.median, g.q3, g.max, ...g.outliers]));
            return (
              <g key={g.model}>
                <text x={LABEL_W - 10} y={cy + 4} textAnchor="end" fontSize={11} fill="var(--text-body)" fontFamily="var(--font-mono)">
                  {truncate(g.model, 16)}
                </text>
                {rawPoints.map((v, vi) => {
                  // Small deterministic vertical jitter so overlapping/adjacent values are
                  // visually separable without touching the shared x-scale.
                  const jitter = ((vi % 3) - 1) * 4;
                  return (
                    <circle key={vi} cx={scale(v)} cy={cy + jitter} r={5} fill={color} fillOpacity={0.85} stroke="var(--bg-panel)" strokeWidth={1.5}>
                      <title>{`${g.model}: ${formatValue(v)}`}</title>
                    </circle>
                  );
                })}
                <text x={scale(Math.max(...rawPoints)) + 12} y={cy + 4} fontSize={11} fill="var(--flux-amber)" fontFamily="var(--font-mono)">
                  {`⚠ n=${g.n}`}
                </text>
                <title>{`${g.model} · n=${g.n} (< 5 — individual values shown, not a box) · range ${formatValue(g.min)}–${formatValue(g.max)}`}</title>
              </g>
            );
          }

          return (
            <g key={g.model}>
              {/* model label */}
              <text x={LABEL_W - 10} y={cy + 4} textAnchor="end" fontSize={11} fill="var(--text-body)" fontFamily="var(--font-mono)">
                {truncate(g.model, 16)}
              </text>
              {/* whisker */}
              <line x1={scale(g.min)} y1={cy} x2={scale(g.max)} y2={cy} stroke={color} strokeWidth={1.5} opacity={0.7} />
              <line x1={scale(g.min)} y1={cy - 5} x2={scale(g.min)} y2={cy + 5} stroke={color} strokeWidth={1.5} opacity={0.7} />
              <line x1={scale(g.max)} y1={cy - 5} x2={scale(g.max)} y2={cy + 5} stroke={color} strokeWidth={1.5} opacity={0.7} />
              {/* IQR box */}
              <rect
                x={scale(g.q1)}
                y={cy - BOX_H / 2}
                width={Math.max(scale(g.q3) - scale(g.q1), 1)}
                height={BOX_H}
                rx={2}
                fill={color}
                fillOpacity={0.22}
                stroke={color}
                strokeWidth={1.5}
              />
              {/* median */}
              <line x1={scale(g.median)} y1={cy - BOX_H / 2} x2={scale(g.median)} y2={cy + BOX_H / 2} stroke={color} strokeWidth={2} />
              {/* outliers */}
              {g.outliers.map((o, oi) => (
                <circle key={oi} cx={scale(o)} cy={cy} r={3} fill="var(--flux-rose)" fillOpacity={0.85}>
                  <title>{`outlier ${formatValue(o)}`}</title>
                </circle>
              ))}
              <title>{`${g.model} · median ${formatValue(g.median)} · IQR ${formatValue(g.q1)}–${formatValue(g.q3)} · n=${g.n}`}</title>
            </g>
          );
        })}
      </svg>
    </div>
  );
}

function computeDomain(groups: BoxGroupVM[]): { min: number; max: number } {
  if (groups.length === 0) return { min: 0, max: 1 };
  let min = Infinity;
  let max = -Infinity;
  for (const g of groups) {
    min = Math.min(min, g.min, ...g.outliers);
    max = Math.max(max, g.max, ...g.outliers);
  }
  if (!Number.isFinite(min) || !Number.isFinite(max)) return { min: 0, max: 1 };
  if (min === max) return { min: min - 1, max: max + 1 };
  const pad = (max - min) * 0.05;
  return { min: min - pad, max: max + pad };
}

/** ~`count` evenly spaced round-ish tick values across [min,max]. */
export function axisTicks(min: number, max: number, count: number): number[] {
  if (min === max) return [min];
  const step = (max - min) / count;
  const out: number[] = [];
  for (let i = 0; i <= count; i++) out.push(Math.round((min + step * i) * 1000) / 1000);
  return out;
}

function truncate(s: string, n: number): string {
  return s.length > n ? `${s.slice(0, n - 1)}…` : s;
}
