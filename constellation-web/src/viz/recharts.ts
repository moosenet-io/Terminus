// CONST-17: the ONE sanctioned door to recharts primitives. "Panels never import
// nivo/recharts directly — they import from src/viz" (§4.1/§9); this barrel is what makes
// that rule mechanically true for the existing recharts-based charts (Analytics,
// EnrichmentAnalytics, CostChart, TokenUsageChart) without rewriting their composition —
// callers use these exactly like the recharts originals, just via this re-export.
//
// CONST-20 addition: AreaChart/Area (Muse watch-history stacked area) and ScatterChart/
// Scatter/ZAxis (Muse taste-cluster map, §5.4). Recharts' own Scatter form is used here
// rather than pulling in the pinned-but-unused `@nivo/scatterplot` foundation — the README
// (§ "The viz kit") notes CONST-17 shipped nivo as foundation only, with the actual chart-form
// wrapper components landing "with the routes that use them (MINT/Models, CONST-22..24)";
// Muse's cluster scatter doesn't need nivo's lazy-loaded `viz` chunk machinery to satisfy its
// §4.4 floor (hover/tooltip/table-twin/keyboard), so it stays on Recharts like every other
// chart in this barrel today. Still governed by the same 4-series all-pairs cap (§4.2)
// regardless of library.
//
// LGUI-09 addition: RadarChart/PolarGrid/PolarAngleAxis/PolarRadiusAxis/Radar — the trait
// radar (§3.4/§8, "4-axis radar thumbnail ... mirrors the sliders"). No radar wrapper existed
// in this barrel yet as of this build (CONST-22, which was expected to add the general
// all-pairs radar/boxplot/heatmap forms, is unmerged) — same Recharts-not-nivo call as the
// scatter addition above, for the same reason (this chart doesn't need nivo's lazy `viz`
// chunk to satisfy the §4.4 floor). See `RadarChart.tsx` for the kit wrapper built on these.
//
// CONST-23/24 reconciliation addition: ReferenceLine/ReferenceDot/ComposedChart — needed by
// the MINT Context-degradation chart (max_context_safe hairlines + OOM markers on a line
// chart), folded into CategoryReportPanel's Context section during the CGUI-10/CONST-23/24
// merge reconciliation.
export {
  LineChart,
  Line,
  AreaChart,
  Area,
  BarChart,
  Bar,
  ScatterChart,
  Scatter,
  ZAxis,
  RadarChart as RechartsRadarChart,
  PolarGrid,
  PolarAngleAxis,
  PolarRadiusAxis,
  Radar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  Legend,
  ResponsiveContainer,
  Cell,
  ReferenceLine,
  ReferenceDot,
  ComposedChart,
} from 'recharts';
