// CONST-17: the viz kit's theme bridge (§4.2). Reads the CSS custom properties at mount
// (one getComputedStyle pass, memoized) and produces the nivo theme + Recharts style
// constants. Solid 1px gridlines everywhere (retires harmony-web's dashed
// `strokeDasharray:'3 3'` — the documented anti-pattern, audit §1.4). Tooltip chrome is
// built separately in ChartTooltip.tsx (uses the same tokens).
//
// EDGE CASE (§10 CONST-17): CSS vars are unresolved in non-browser test environments
// (jsdom/vitest with no real stylesheet applied). FALLBACK_HEXES below are the documented
// fallback for that case ONLY — never reference them from panel code, they exist so
// theme.ts itself doesn't throw when getComputedStyle returns ''.

const FALLBACK_HEXES = {
  textMuted: '#9CA3AF',
  fontMono: "'JetBrains Mono', ui-monospace, 'SF Mono', Menlo, monospace",
  chartAxis: '#2C2350',
  chartGrid: '#221A40',
  bgElevated: '#1A1333',
  border: 'rgba(168,85,247,0.22)',
  shadowLg: '0 20px 60px rgba(0,0,0,0.55)',
  textBody: '#C7C3D6',
} as const;

export interface VizTheme {
  tickLabel: { fontSize: number; fill: string; fontFamily: string };
  axisLine: string;
  gridLine: string;
  tooltipBg: string;
  tooltipBorder: string;
  tooltipShadow: string;
  textMuted: string;
  fontMono: string;
  /** nivo theme object — shared shape across radar/boxplot/heatmap/parallel-coords/swarm/scatter */
  nivo: Record<string, unknown>;
}

let cached: VizTheme | null = null;

function readVar(name: string, fallback: string): string {
  if (typeof window === 'undefined' || typeof document === 'undefined') return fallback;
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

/** Memoized: computed once per page load. Call `resetVizTheme()` in tests that swap themes. */
export function getVizTheme(): VizTheme {
  if (cached) return cached;

  const textMuted = readVar('--text-muted', FALLBACK_HEXES.textMuted);
  const fontMono = readVar('--font-mono', FALLBACK_HEXES.fontMono);
  const chartAxis = readVar('--chart-axis', FALLBACK_HEXES.chartAxis);
  const chartGrid = readVar('--chart-grid', FALLBACK_HEXES.chartGrid);
  const bgElevated = readVar('--bg-elevated', FALLBACK_HEXES.bgElevated);
  const border = readVar('--border', FALLBACK_HEXES.border);
  const shadowLg = readVar('--shadow-lg', FALLBACK_HEXES.shadowLg);
  const textBody = readVar('--text-body', FALLBACK_HEXES.textBody);

  const tickLabel = { fontSize: 11, fill: textMuted, fontFamily: fontMono };

  const nivo = {
    background: 'transparent',
    text: { fontSize: 11, fill: textMuted, fontFamily: fontMono },
    axis: {
      domain: { line: { stroke: chartAxis, strokeWidth: 1 } },
      ticks: {
        line: { stroke: chartAxis, strokeWidth: 1 },
        text: { fontSize: 11, fill: textMuted, fontFamily: fontMono },
      },
      legend: { text: { fontSize: 11, fill: textBody, fontFamily: fontMono } },
    },
    grid: { line: { stroke: chartGrid, strokeWidth: 1 } },
    tooltip: {
      container: {
        background: bgElevated,
        border: `1px solid ${border}`,
        borderRadius: 8,
        boxShadow: shadowLg,
        color: textBody,
        fontSize: 12,
      },
    },
    labels: { text: { fontSize: 11, fill: textBody, fontFamily: fontMono } },
    legends: { text: { fontSize: 11, fill: textBody, fontFamily: fontMono } },
    crosshair: { line: { stroke: chartAxis, strokeWidth: 1 } },
  };

  cached = {
    tickLabel,
    axisLine: chartAxis,
    gridLine: chartGrid,
    tooltipBg: bgElevated,
    tooltipBorder: border,
    tooltipShadow: shadowLg,
    textMuted,
    fontMono,
    nivo,
  };
  return cached;
}

/** Test-only: clears the memoized theme so a subsequent getVizTheme() re-reads the DOM. */
export function resetVizTheme(): void {
  cached = null;
}

/** Recharts shared style constants — the solid-gridline replacement for harmony-web's
 *  GRID_PROPS = {strokeDasharray:'3 3', ...}. Import these instead of redefining per-chart.
 *  `strokeDasharray` is deliberately OMITTED (not set to 'none') — Recharts' default is
 *  already a solid line, and an explicit 'none' string is a no-op that reads like a residual
 *  of the anti-pattern it's replacing. */
export function rechartsGridProps() {
  const t = getVizTheme();
  return { stroke: t.gridLine } as const;
}

export function rechartsTickStyle() {
  const t = getVizTheme();
  return { fontSize: 11, fill: t.textMuted, fontFamily: t.fontMono } as const;
}

export function rechartsTooltipStyle(): React.CSSProperties {
  const t = getVizTheme();
  return {
    background: t.tooltipBg,
    border: `1px solid ${t.tooltipBorder}`,
    borderRadius: 8,
    boxShadow: t.tooltipShadow,
    fontSize: 12,
    fontFamily: t.fontMono,
  };
}
