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
export {
  LineChart,
  Line,
  BarChart,
  Bar,
  AreaChart,
  Area,
  ScatterChart,
  Scatter,
  ZAxis,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  Legend,
  ResponsiveContainer,
  Cell,
} from 'recharts';
