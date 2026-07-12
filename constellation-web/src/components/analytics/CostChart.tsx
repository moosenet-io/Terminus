// WIRE-06: Dual line chart — actual cost vs imputed cloud cost
import { LineChart, Line, XAxis, YAxis, Tooltip, ResponsiveContainer, Legend } from 'recharts';
import type { CostData } from '../../hooks/useChordAnalytics';

interface Props { data: CostData[]; }

export function CostChart({ data }: Props) {
  if (data.length === 0) {
    return (
      <div style={{ padding: 16, textAlign: 'center', color: 'var(--text-tertiary)' }}>
        No cost data yet
      </div>
    );
  }
  return (
    <ResponsiveContainer width="100%" height={200}>
      <LineChart data={data}>
        <XAxis dataKey="date" tick={{ fontSize: 10 }} />
        <YAxis tick={{ fontSize: 10 }} />
        <Tooltip />
        <Legend />
        <Line type="monotone" dataKey="actual_cost" name="Actual Cost ($)" stroke="#22c55e" />
        <Line type="monotone" dataKey="imputed_cost" name="Cloud Would Cost ($)" stroke="#f97316" strokeDasharray="5 5" />
      </LineChart>
    </ResponsiveContainer>
  );
}
