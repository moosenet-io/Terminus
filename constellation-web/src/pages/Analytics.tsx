// SRPT: Soma Analytics page — recharts-based observability charts.
// TRIAGE-09: Enrichment quality section added at bottom.
// WIRE-06: Chord savings hero, token usage, and cost charts added at top.
import { useState, useEffect } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import { useEscalationData } from '../hooks/useEscalationData';
import { EnrichmentAnalytics } from '../components/EnrichmentAnalytics';
import { useChordAnalytics } from '../hooks/useChordAnalytics';
import { SavingsHero } from '../components/analytics/SavingsHero';
import { TokenUsageChart } from '../components/analytics/TokenUsageChart';
import { CostChart } from '../components/analytics/CostChart';
import { ProviderPerformance } from '../components/analytics/ProviderPerformance';
import {
  LineChart, Line, BarChart, Bar,
  XAxis, YAxis, CartesianGrid, Tooltip, Legend, ResponsiveContainer,
} from '../viz/recharts';
// CONST-17: solid hairline grid + brand tick style from the viz kit — retires the dashed
// GRID_PROPS anti-pattern (audit §1.4).
import { rechartsGridProps, rechartsTickStyle } from '../viz/theme';
// CONST-17 r3: every chart ships its table-view twin (§4.4) — the inline SRPT charts below
// were the last holdouts (EnrichmentAnalytics/CostChart/TokenUsageChart got theirs in r1).
import { TableView, TableViewControls, useTableView } from '../viz/TableViewToggle';
import type { DataTableColumn } from '../components/DataTable';

type Period = '24h' | '7d' | '30d';

interface CompletionBucket { hour: string; completed: number; failed: number; }
interface ProviderStat { name: string; avg_latency_ms: number; success_rate: number; avg_cost: number; avg_quality: number; task_count: number; }
interface CostBucket { date: string; cost_usd: number; provider: string; }
interface DurationBucket { range: string; count: number; tier: string; }
interface QualityPoint { provider: string; score: number; task_id: string; }

// ── Shared chart theme (CONST-17: brand-derived, no dashed grid) ──────────────
const CHART_STYLE = rechartsTickStyle();
const GRID_PROPS = rechartsGridProps();

/** CONST-17 r3: shared chart/table twin wrapper for the inline SRPT charts — toggle row
 *  sits ABOVE the chart box (never inside a fixed-height body, per the r2 clipping fix). */
function TwinChart<T>({ columns, rows, rowKey, children }: {
  columns: DataTableColumn<T>[];
  rows: T[];
  rowKey: (row: T, index: number) => string;
  children: React.ReactNode;
}) {
  const { view, setView } = useTableView();
  return (
    <div>
      <div style={{ display: 'flex', justifyContent: 'flex-end', marginBottom: 6 }}>
        <TableViewControls view={view} onChange={setView} />
      </div>
      <TableView view={view} columns={columns} rows={rows} rowKey={rowKey}>
        {children}
      </TableView>
    </div>
  );
}

function EmptyState({ message }: { message: string }) {
  return (
    <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', height: 200, color: 'var(--h-text-muted)', fontSize: 13 }}>
      {message}
    </div>
  );
}

// ── SRPT-01: Task Completion Rate ─────────────────────────────────────────────
function TaskCompletionChart({ period, project }: { period: Period; project: string }) {
  const [data, setData] = useState<CompletionBucket[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    setLoading(true);
    const params = new URLSearchParams({ period, ...(project ? { project } : {}) });
    getAggregationClient()
      .request<CompletionBucket[]>('harmony', `/analytics/completion-rate?${params}`)
      .then(d => { setData(d); setLoading(false); })
      .catch(() => setLoading(false));
  }, [period, project]);

  if (loading) return <div className="h-skeleton" style={{ height: 200 }} />;
  if (!data.length) return <EmptyState message="No data for this period" />;

  const columns: DataTableColumn<CompletionBucket>[] = [
    { key: 'hour', header: 'Hour', render: r => r.hour },
    { key: 'completed', header: 'Completed', align: 'right', render: r => r.completed },
    { key: 'failed', header: 'Failed', align: 'right', render: r => r.failed },
  ];

  return (
    <TwinChart columns={columns} rows={data} rowKey={r => r.hour}>
      <ResponsiveContainer width="100%" height={200}>
        <LineChart data={data} margin={{ top: 5, right: 20, bottom: 5, left: 0 }}>
          <CartesianGrid {...GRID_PROPS} />
          <XAxis dataKey="hour" tick={CHART_STYLE} tickFormatter={s => s.slice(11, 16)} />
          <YAxis tick={CHART_STYLE} />
          <Tooltip contentStyle={{ background: 'var(--h-bg-card)', border: '1px solid var(--h-border)', fontSize: 11 }} />
          <Legend wrapperStyle={{ fontSize: 11 }} />
          <Line type="monotone" dataKey="completed" stroke="var(--h-green)" strokeWidth={2} dot={false} name="Completed" />
          <Line type="monotone" dataKey="failed" stroke="var(--h-red)" strokeWidth={1.5} dot={false} name="Failed" />
        </LineChart>
      </ResponsiveContainer>
    </TwinChart>
  );
}

// ── SRPT-02: Provider Performance ─────────────────────────────────────────────
function ProviderComparisonChart({ period }: { period: Period }) {
  const [data, setData] = useState<ProviderStat[]>([]);
  const [metric, setMetric] = useState<'avg_latency_ms' | 'success_rate' | 'avg_cost' | 'avg_quality'>('success_rate');
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    getAggregationClient()
      .request<ProviderStat[]>('harmony', `/analytics/provider-comparison?period=${period}`)
      .then(d => { setData(d); setLoading(false); })
      .catch(() => setLoading(false));
  }, [period]);

  const metricLabels = { avg_latency_ms: 'Latency (ms)', success_rate: 'Success Rate', avg_cost: 'Avg Cost ($)', avg_quality: 'Quality Score' };
  const metricColors: Record<string, string> = { avg_latency_ms: 'var(--h-amber)', success_rate: 'var(--h-green)', avg_cost: 'var(--h-red)', avg_quality: 'var(--h-teal)' };

  if (loading) return <div className="h-skeleton" style={{ height: 200 }} />;
  if (!data.length) return <EmptyState message="No provider data" />;

  return (
    <div>
      <div style={{ display: 'flex', gap: 6, marginBottom: 8 }}>
        {Object.entries(metricLabels).map(([k, label]) => (
          <button key={k} onClick={() => setMetric(k as typeof metric)}
            style={{ fontSize: 11, padding: '2px 8px', borderRadius: 4, border: `1px solid ${metric === k ? 'var(--h-teal)' : 'var(--h-border)'}`, background: metric === k ? 'var(--h-bg-active)' : 'var(--h-bg-card)', color: metric === k ? 'var(--h-teal)' : 'var(--h-text-dim)', cursor: 'pointer' }}>
            {label}
          </button>
        ))}
      </div>
      <TwinChart
        columns={[
          { key: 'name', header: 'Provider', render: r => r.name },
          { key: 'metric', header: metricLabels[metric], align: 'right', render: r => String(r[metric]) },
          { key: 'task_count', header: 'Tasks', align: 'right', render: r => r.task_count },
        ] as DataTableColumn<ProviderStat>[]}
        rows={data}
        rowKey={r => r.name}
      >
        <ResponsiveContainer width="100%" height={180}>
          <BarChart data={data} margin={{ top: 5, right: 20, bottom: 5, left: 0 }}>
            <CartesianGrid {...GRID_PROPS} />
            <XAxis dataKey="name" tick={CHART_STYLE} />
            <YAxis tick={CHART_STYLE} />
            <Tooltip contentStyle={{ background: 'var(--h-bg-card)', border: '1px solid var(--h-border)', fontSize: 11 }} />
            <Bar dataKey={metric} fill={metricColors[metric]} name={metricLabels[metric]} />
          </BarChart>
        </ResponsiveContainer>
      </TwinChart>
    </div>
  );
}

// ── SRPT-03: Cost Tracking ────────────────────────────────────────────────────
function CostTrackingChart({ period }: { period: Period }) {
  const [data, setData] = useState<CostBucket[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    getAggregationClient()
      .request<CostBucket[]>('harmony', `/analytics/cost-tracking?period=${period}`)
      .then(d => { setData(d); setLoading(false); })
      .catch(() => setLoading(false));
  }, [period]);

  if (loading) return <div className="h-skeleton" style={{ height: 160 }} />;
  if (!data.length) return <EmptyState message="No cost data" />;

  const columns: DataTableColumn<CostBucket>[] = [
    { key: 'date', header: 'Date', render: r => r.date },
    { key: 'cost_usd', header: 'Cost (USD)', align: 'right', render: r => `$${r.cost_usd.toFixed(4)}` },
  ];

  return (
    <TwinChart columns={columns} rows={data} rowKey={(r, i) => `${r.date}-${i}`}>
      <ResponsiveContainer width="100%" height={160}>
        <LineChart data={data} margin={{ top: 5, right: 20, bottom: 5, left: 0 }}>
          <CartesianGrid {...GRID_PROPS} />
          <XAxis dataKey="date" tick={CHART_STYLE} />
          <YAxis tick={CHART_STYLE} tickFormatter={v => `$${v.toFixed(2)}`} />
          <Tooltip contentStyle={{ background: 'var(--h-bg-card)', border: '1px solid var(--h-border)', fontSize: 11 }} formatter={(v) => [`$${Number(v).toFixed(4)}`, 'Cost']} />
          <Line type="monotone" dataKey="cost_usd" stroke="var(--h-amber)" strokeWidth={2} dot={false} name="Daily Cost" />
        </LineChart>
      </ResponsiveContainer>
    </TwinChart>
  );
}

// ── SRPT-04: Build Duration Histogram ─────────────────────────────────────────
function BuildDurationChart({ period }: { period: Period }) {
  const [data, setData] = useState<DurationBucket[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    getAggregationClient()
      .request<DurationBucket[]>('harmony', `/analytics/build-duration?period=${period}`)
      .then(d => { setData(d); setLoading(false); })
      .catch(() => setLoading(false));
  }, [period]);

  if (loading) return <div className="h-skeleton" style={{ height: 160 }} />;
  if (!data.length) return <EmptyState message="No build duration data" />;

  const columns: DataTableColumn<DurationBucket>[] = [
    { key: 'range', header: 'Duration', render: r => r.range },
    { key: 'tier', header: 'Tier', render: r => r.tier },
    { key: 'count', header: 'Tasks', align: 'right', render: r => r.count },
  ];

  return (
    <TwinChart columns={columns} rows={data} rowKey={(r, i) => `${r.range}-${r.tier}-${i}`}>
      <ResponsiveContainer width="100%" height={160}>
        <BarChart data={data} margin={{ top: 5, right: 20, bottom: 5, left: 0 }}>
          <CartesianGrid {...GRID_PROPS} />
          <XAxis dataKey="range" tick={CHART_STYLE} />
          <YAxis tick={CHART_STYLE} />
          <Tooltip contentStyle={{ background: 'var(--h-bg-card)', border: '1px solid var(--h-border)', fontSize: 11 }} />
          <Bar dataKey="count" fill="var(--h-blue)" name="Tasks" />
        </BarChart>
      </ResponsiveContainer>
    </TwinChart>
  );
}

// ── SRPT-05: Quality Score Distribution ──────────────────────────────────────
function QualityScoreChart({ period }: { period: Period }) {
  const [data, setData] = useState<QualityPoint[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    getAggregationClient()
      .request<QualityPoint[]>('harmony', `/analytics/quality-scores?period=${period}`)
      .then(d => { setData(d); setLoading(false); })
      .catch(() => setLoading(false));
  }, [period]);

  // Group by provider and show avg
  const byProvider = data.reduce((acc, p) => {
    if (!acc[p.provider]) acc[p.provider] = [];
    acc[p.provider].push(p.score);
    return acc;
  }, {} as Record<string, number[]>);
  const chartData = Object.entries(byProvider).map(([name, scores]) => ({
    name,
    avg_quality: scores.reduce((a, b) => a + b, 0) / scores.length,
  }));

  if (loading) return <div className="h-skeleton" style={{ height: 160 }} />;
  if (!chartData.length) return <EmptyState message="No quality data" />;

  const columns: DataTableColumn<{ name: string; avg_quality: number }>[] = [
    { key: 'name', header: 'Provider', render: r => r.name },
    { key: 'avg_quality', header: 'Avg Quality', align: 'right', render: r => r.avg_quality.toFixed(2) },
  ];

  return (
    <TwinChart columns={columns} rows={chartData} rowKey={r => r.name}>
      <ResponsiveContainer width="100%" height={160}>
        <BarChart data={chartData} margin={{ top: 5, right: 20, bottom: 5, left: 0 }}>
          <CartesianGrid {...GRID_PROPS} />
          <XAxis dataKey="name" tick={CHART_STYLE} />
          <YAxis tick={CHART_STYLE} domain={[0, 1]} tickFormatter={v => v.toFixed(1)} />
          <Tooltip contentStyle={{ background: 'var(--h-bg-card)', border: '1px solid var(--h-border)', fontSize: 11 }} formatter={(v) => [Number(v).toFixed(2), 'Avg Quality']} />
          <Bar dataKey="avg_quality" fill="var(--h-teal)" name="Avg Quality" />
        </BarChart>
      </ResponsiveContainer>
    </TwinChart>
  );
}

// ── SRPT-06: Analytics page layout ───────────────────────────────────────────
export function Analytics() {
  const [period, setPeriod] = useState<Period>('7d');
  const [chordPeriod, setChordPeriod] = useState('30d');
  const [project, setProject] = useState('');
  const { data: escalationData, loading: escalationLoading, error: escalationError } = useEscalationData();
  const { savings, costData, loading: chordLoading } = useChordAnalytics(chordPeriod);

  const periods: Period[] = ['24h', '7d', '30d'];
  const chordPeriods = ['7d', '30d', 'all'];

  return (
    <div style={{ padding: 16, overflowY: 'auto', height: '100%' }}>
      {/* WIRE-06: Chord savings section */}
      <div style={{ marginBottom: 20 }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginBottom: 10 }}>
          <h3 style={{ fontSize: 14, fontWeight: 600, color: 'var(--h-teal)', margin: 0 }}>Inference Savings</h3>
          <div style={{ display: 'flex', gap: 6, marginLeft: 'auto' }}>
            {chordPeriods.map(p => (
              <button key={p} onClick={() => setChordPeriod(p)} style={{
                fontSize: 11, padding: '2px 8px', borderRadius: 4,
                border: `1px solid ${chordPeriod === p ? 'var(--h-teal)' : 'var(--h-border)'}`,
                background: chordPeriod === p ? 'var(--h-bg-active)' : 'var(--h-bg-card)',
                color: chordPeriod === p ? 'var(--h-teal)' : 'var(--h-text-dim)', cursor: 'pointer',
              }}>{p}</button>
            ))}
          </div>
        </div>
        {chordLoading ? (
          <div className="h-skeleton" style={{ height: 120, borderRadius: 8, marginBottom: 12 }} />
        ) : (
          <SavingsHero data={savings} />
        )}
        <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12, marginTop: 12 }}>
          <div className="h-card">
            <div className="h-card-header" style={{ cursor: 'default' }}>
              <span style={{ fontWeight: 600, fontSize: 13 }}>Token Usage</span>
            </div>
            <div className="h-card-body">
              {chordLoading ? <div className="h-skeleton" style={{ height: 200 }} /> : <TokenUsageChart data={costData} />}
            </div>
          </div>
          <div className="h-card">
            <div className="h-card-header" style={{ cursor: 'default' }}>
              <span style={{ fontWeight: 600, fontSize: 13 }}>Cost vs. Cloud Imputed</span>
            </div>
            <div className="h-card-body">
              {chordLoading ? <div className="h-skeleton" style={{ height: 200 }} /> : <CostChart data={costData} />}
            </div>
          </div>
        </div>
        <div style={{ marginTop: 12 }}>
          <ProviderPerformance />
        </div>
      </div>

      {/* Header + controls */}
      <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginBottom: 16 }}>
        <h2 style={{ fontSize: 16, fontWeight: 600, color: 'var(--h-teal)', margin: 0 }}>Analytics</h2>
        <div style={{ display: 'flex', gap: 6, marginLeft: 'auto' }}>
          {periods.map(p => (
            <button key={p} onClick={() => setPeriod(p)} style={{
              fontSize: 11, padding: '3px 10px', borderRadius: 4,
              border: `1px solid ${period === p ? 'var(--h-teal)' : 'var(--h-border)'}`,
              background: period === p ? 'var(--h-bg-active)' : 'var(--h-bg-card)',
              color: period === p ? 'var(--h-teal)' : 'var(--h-text-dim)', cursor: 'pointer',
            }}>{p}</button>
          ))}
        </div>
      </div>

      {/* Grid of charts */}
      <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 }}>
        {/* Full-width completion chart */}
        <div className="h-card" style={{ gridColumn: '1 / -1' }}>
          <div className="h-card-header" style={{ cursor: 'default' }}>
            <span style={{ fontWeight: 600, fontSize: 13 }}>Task Completion Rate</span>
            <input value={project} onChange={e => setProject(e.target.value)} placeholder="Filter by project (e.g. LM)"
              style={{ background: 'var(--h-bg-card)', border: '1px solid var(--h-border)', borderRadius: 4, color: 'var(--h-text)', padding: '2px 8px', fontSize: 11, outline: 'none' }} />
          </div>
          <div className="h-card-body"><TaskCompletionChart period={period} project={project} /></div>
        </div>

        {/* Provider comparison */}
        <div className="h-card">
          <div className="h-card-header" style={{ cursor: 'default' }}><span style={{ fontWeight: 600, fontSize: 13 }}>Provider Performance</span></div>
          <div className="h-card-body"><ProviderComparisonChart period={period} /></div>
        </div>

        {/* Cost tracking */}
        <div className="h-card">
          <div className="h-card-header" style={{ cursor: 'default' }}><span style={{ fontWeight: 600, fontSize: 13 }}>Daily Cost</span></div>
          <div className="h-card-body"><CostTrackingChart period={period} /></div>
        </div>

        {/* Build duration */}
        <div className="h-card">
          <div className="h-card-header" style={{ cursor: 'default' }}><span style={{ fontWeight: 600, fontSize: 13 }}>Build Duration</span></div>
          <div className="h-card-body"><BuildDurationChart period={period} /></div>
        </div>

        {/* Quality scores */}
        <div className="h-card">
          <div className="h-card-header" style={{ cursor: 'default' }}><span style={{ fontWeight: 600, fontSize: 13 }}>Quality Scores</span></div>
          <div className="h-card-body"><QualityScoreChart period={period} /></div>
        </div>
      </div>

      {/* TRIAGE-09: Enrichment quality section */}
      <div style={{ marginTop: 24 }}>
        <h3 style={{ fontSize: 14, fontWeight: 600, color: 'var(--h-teal)', marginBottom: 12 }}>
          Enrichment Quality (Escalation Analytics)
        </h3>
        <div className="h-card">
          <div className="h-card-body">
            {escalationLoading && <div style={{ color: 'var(--text-tertiary)', padding: 16 }}>Loading escalation data…</div>}
            {escalationError && <div style={{ color: 'var(--status-error)', padding: 16 }}>Error: {escalationError}</div>}
            {escalationData && <EnrichmentAnalytics analytics={escalationData} />}
          </div>
        </div>
      </div>
    </div>
  );
}
