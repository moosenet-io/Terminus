// WIRE-06: Stacked bar chart of local vs cloud token usage over time
import { BarChart, Bar, XAxis, YAxis, Tooltip, ResponsiveContainer, Legend } from 'recharts';
import type { CostData } from '../../hooks/useChordAnalytics';

interface Props { data: CostData[]; }

export function TokenUsageChart({ data }: Props) {
  if (data.length === 0) {
    return (
      <div style={{ padding: 16, textAlign: 'center', color: 'var(--text-tertiary)' }}>
        No token data yet
      </div>
    );
  }
  return (
    <ResponsiveContainer width="100%" height={200}>
      <BarChart data={data}>
        <XAxis dataKey="date" tick={{ fontSize: 10 }} />
        <YAxis tick={{ fontSize: 10 }} />
        <Tooltip />
        <Legend />
        <Bar dataKey="tokens_local" name="Local" fill="#3b82f6" stackId="a" />
        <Bar dataKey="tokens_cloud" name="Cloud" fill="#f97316" stackId="a" />
      </BarChart>
    </ResponsiveContainer>
  );
}
