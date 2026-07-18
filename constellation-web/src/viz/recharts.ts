// CONST-17: the ONE sanctioned door to recharts primitives. "Panels never import
// nivo/recharts directly — they import from src/viz" (§4.1/§9); this barrel is what makes
// that rule mechanically true for the existing recharts-based charts (Analytics,
// EnrichmentAnalytics, CostChart, TokenUsageChart) without rewriting their composition —
// callers use these exactly like the recharts originals, just via this re-export.
export {
  LineChart,
  Line,
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  Legend,
  ResponsiveContainer,
  Cell,
} from 'recharts';
