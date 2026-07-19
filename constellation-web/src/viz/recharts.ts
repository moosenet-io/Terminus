// CONST-17: the ONE sanctioned door to recharts primitives. "Panels never import
// nivo/recharts directly — they import from src/viz" (§4.1/§9); this barrel is what makes
// that rule mechanically true for the existing recharts-based charts (Analytics,
// EnrichmentAnalytics, CostChart, TokenUsageChart) without rewriting their composition —
// callers use these exactly like the recharts originals, just via this re-export.
// CONST-23: extended for C7 (context degradation lines + OOM markers + max_context_safe
// hairline) and C8 (stacked area + epoch marker hairlines) — AreaChart/Area for the stacked
// fills, ReferenceLine for hairlines, Scatter/ReferenceDot for OOM markers on a line chart.
export {
  LineChart,
  Line,
  BarChart,
  Bar,
  AreaChart,
  Area,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  Legend,
  ResponsiveContainer,
  Cell,
  ReferenceLine,
  ReferenceDot,
  Scatter,
  ComposedChart,
} from 'recharts';
