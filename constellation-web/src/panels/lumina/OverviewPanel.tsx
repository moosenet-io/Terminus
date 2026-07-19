// LGUI-06: the Lumina module's Overview panel (`lumina.overview`, route `/lumina`, §3.1 of
// LUMINA-GUI-SPEC.md). Identity card, metric tile row, three charts (memory growth / routing
// mix / top tools), a log-line activity feed, first-run redirect (or fallback hero card) for
// onboarding, and a whole-panel degraded card when lumina's `/api/health` entry is down. All
// data comes from `useLumina` (composes the §7 endpoints through the aggregation client, see
// that hook's doc).
//
// First-run (§2 of the spec): when `status.onboarding_complete === false`, the intent is to
// land the admin on `/lumina/setup` (LGUI-12's wizard route). LGUI-12 hasn't merged in this
// build, so that route ISN'T registered yet. Review fix: redirecting unconditionally to an
// unregistered route just bounces off App.tsx's wildcard Route back to `/overview`, making the
// "NEW · needs setup" card permanently unreachable dead code. Instead this panel checks the
// panel registry dynamically (`isPanelAvailable('lumina.setup')`, moduleRegistry.ts) and only
// redirects when the target route actually exists; otherwise it renders the needs-setup hero
// card here on `/lumina` (reachable now) with its "Begin setup" action disabled + annotated.
// The redirect self-activates the moment LGUI-12 registers `lumina.setup` -- zero code change.
import { useMemo } from 'react';
import { Navigate } from 'react-router-dom';
import { Card } from '../../components/Card';
import { Badge } from '../../components/Badge';
import { MetricCard } from '../../components/MetricCard';
import { ChartCard } from '../../viz/ChartCard';
import { ChartEmpty } from '../../viz/ChartEmpty';
import { ChartLegend } from '../../viz/ChartLegend';
import { TableView, TableViewControls, useTableView } from '../../viz/TableViewToggle';
import { rechartsGridProps, rechartsTickStyle, rechartsTooltipStyle } from '../../viz/theme';
import { CATEGORICAL_HEX } from '../../viz/palette';
import {
  AreaChart,
  Area,
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
} from '../../viz/recharts';
import { useLumina } from '../../hooks/useLumina';
import { isPanelAvailable } from '../../lib/moduleRegistry';
import { IdentityCard } from './IdentityCard';
import type { LuminaAnalyticsEvent } from '../../types/lumina';

const LEVEL_COLOR: Record<LuminaAnalyticsEvent['level'], string> = {
  ok: 'var(--status-success)',
  warn: 'var(--status-warning)',
  error: 'var(--status-error)',
};

function formatTime(ts: string): string {
  const d = new Date(ts);
  if (Number.isNaN(d.getTime())) return ts;
  return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = bytes / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i += 1; }
  return `${v.toFixed(1)} ${units[i]}`;
}

const CARD_STYLE: React.CSSProperties = {
  background: 'var(--grad-card)',
  border: '1px solid var(--border)',
  borderRadius: 'var(--radius-lg)',
  boxShadow: 'var(--shadow-md), var(--inset-hi)',
  padding: 'var(--space-4)',
};

export function OverviewPanel() {
  const { status, engram, analyticsRouting, analyticsTools, events, health, degraded, refetchAll } = useLumina();

  // Table-view twin state (§4.4: "every chart has a table view") -- one per chart, hooks called
  // unconditionally ahead of the degraded/first-run early returns below.
  const memoryGrowthTable = useTableView();
  const routingMixTable = useTableView();
  const topToolsTable = useTableView();

  // Chart-window review fix: `growth_30d` is OPTIONAL (types/lumina.ts) -- `undefined` (field
  // absent, backend doesn't expose the series) and `[]` (field present, no history yet) are
  // different states and get different ChartEmpty copy below. `.slice(-30)` is a defensive
  // window clamp so this chart never renders more than its declared 30-day window even if a
  // future backend over-sends (mirrors the same discipline applied to the 14d/7d analytics
  // fetches, which are windowed server-side via the `days=` query param instead).
  const memoryGrowthSeriesMissing = engram.data != null && engram.data.growth_30d === undefined;
  const memoryGrowthData = (engram.data?.growth_30d ?? []).slice(-30);
  // Routing mix (§3.1: 14-day) reads analyticsRouting (fetched with days=14) -- NOT
  // analyticsTools (days=7). `.slice(-14)` defensively clamps in case a backend sends more.
  const dailyData = (analyticsRouting.data?.daily ?? []).slice(-14);
  // Top tools (§3.1: 7-day) reads analyticsTools (fetched with days=7) -- NOT analyticsRouting.
  const topToolsData = useMemo(
    () => [...(analyticsTools.data?.top_tools ?? [])].sort((a, b) => b.count - a.count).slice(0, 8),
    [analyticsTools.data],
  );
  const eventList = events.data?.events ?? [];

  // Tile-row derived values -- degrade honestly (em-dash) rather than fabricating a number when
  // a section is still loading/errored or the source has nothing to derive it from (§3.1).
  // "Today" reads the routing-mix (14d) window's last entry -- same window the chart shows.
  const memoriesDelta = memoryGrowthData.length >= 2
    ? memoryGrowthData[memoryGrowthData.length - 1].total - memoryGrowthData[memoryGrowthData.length - 2].total
    : null;
  const todayTurns = dailyData.length > 0 ? dailyData[dailyData.length - 1].turns : null;
  const todayDeep = dailyData.length > 0 ? dailyData[dailyData.length - 1].deep : null;
  const deepShare = todayTurns && todayTurns > 0 && todayDeep != null
    ? `${Math.round((todayDeep / todayTurns) * 100)}%`
    : '—';

  // §3.1 whole-panel degraded card: the lumina health entry reports available:false. Rendered
  // AFTER all hooks (rules-of-hooks) but before anything else, per the module-standard degraded
  // convention (icon + detail + retry, §2.6).
  if (degraded) {
    const detail = health.data?.find(h => h.system === 'lumina')?.detail;
    return (
      <div style={{ padding: 'var(--space-5)' }}>
        <Card variant="content">
          <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)', alignItems: 'flex-start' }}>
            <div style={{ fontSize: 'var(--fs-h3)', color: 'var(--status-error)' }}>Lumina unavailable</div>
            <div style={{ color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>
              {detail ?? 'backend not reachable'}
            </div>
            <button
              type="button"
              onClick={refetchAll}
              style={{
                marginTop: 'var(--space-2)',
                background: 'var(--grad-accent)',
                color: 'var(--accent-on)',
                border: 'none',
                borderRadius: 'var(--radius-md)',
                padding: 'var(--space-1) var(--space-3)',
                cursor: 'pointer',
                fontSize: 'var(--fs-sm)',
              }}
            >
              Retry
            </button>
          </div>
        </Card>
      </div>
    );
  }

  // First-run (§2): redirect the module landing to the onboarding wizard -- but ONLY when
  // `lumina.setup` is actually registered (review fix). Checked dynamically against the panel
  // registry rather than assumed; while LGUI-12 is unmerged this is always false, so control
  // falls through to the needsSetup hero card below instead of an unconditional Navigate that
  // just bounces off the shell's wildcard Route back to `/overview`. Only fires once status has
  // actually loaded (never on the still-null first render, matching App.tsx's healthLoaded
  // convention -- an unloaded/unknown state must never look like "needs setup").
  const needsSetup = !status.loading && status.data?.onboarding_complete === false;
  const setupRouteRegistered = isPanelAvailable('lumina.setup');
  if (needsSetup && setupRouteRegistered) {
    return <Navigate to="/lumina/setup" replace />;
  }

  return (
    <div style={{ padding: 'var(--space-5)', overflow: 'auto', flex: 1, display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      {/* seam note (§2, LGUI-06 scope): the shared Overview card canvas
          (panels/overview/ModuleCard.tsx) only has a 4-state CardState union
          ('online'|'idle'|'error'|'disabled') with no per-module state-injection seam today --
          adding a 5th "needs setup" state there is a canvas refactor out of this item's scope
          (spec: "if not, add a clearly-marked seam note; do not refactor the canvas here"). The
          Badge/Button below is this panel's own equivalent, rendered on ITS OWN route (/lumina)
          rather than on the /overview canvas card. When ModuleCard grows a state-injection seam,
          wire `needsSetup` through OverviewPanel(canvas) -> ModuleCard here instead.
          Review fix: this card is now REACHABLE whenever needsSetup is true (previously dead
          code behind an unconditional redirect to an unregistered route). Its "Begin setup"
          action is disabled + annotated until LGUI-12 actually registers `lumina.setup` --
          `setupRouteRegistered` flips this the moment that lands, no code change needed. */}
      {needsSetup && (
        <Card variant="content" style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 'var(--space-3)' }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)' }}>
            <Badge tone="amber" glowDot>NEW · needs setup</Badge>
            <span style={{ color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>
              This assistant hasn't completed onboarding yet.
              {!setupRouteRegistered && ' The setup wizard lands with LGUI-12.'}
            </span>
          </div>
          {setupRouteRegistered ? (
            <a
              href="/lumina/setup"
              style={{
                background: 'var(--grad-accent)',
                color: 'var(--accent-on)',
                borderRadius: 'var(--radius-md)',
                padding: 'var(--space-1) var(--space-3)',
                fontSize: 'var(--fs-sm)',
                textDecoration: 'none',
                fontWeight: 600,
              }}
            >
              Begin setup
            </a>
          ) : (
            <button
              type="button"
              disabled
              title="setup wizard lands with LGUI-12"
              aria-disabled="true"
              style={{
                background: 'var(--surface-2, var(--border))',
                color: 'var(--text-muted)',
                border: 'none',
                borderRadius: 'var(--radius-md)',
                padding: 'var(--space-1) var(--space-3)',
                fontSize: 'var(--fs-sm)',
                fontWeight: 600,
                cursor: 'not-allowed',
              }}
            >
              Begin setup
            </button>
          )}
        </Card>
      )}

      <IdentityCard status={status.data} loading={status.loading} error={status.error} />

      {/* Tile row */}
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(160px, 1fr))', gap: 'var(--space-3)' }}>
        <MetricCard
          label="Memories"
          value={engram.loading ? '…' : engram.data ? `${engram.data.total}${memoriesDelta != null ? ` (+${memoriesDelta}/24h)` : ''}` : '—'}
        />
        <MetricCard label="Turns today" value={analyticsRouting.loading ? '…' : todayTurns != null ? String(todayTurns) : '—'} />
        <MetricCard label="Deep-turn share" value={analyticsRouting.loading ? '…' : deepShare} />
        <MetricCard label="Active users" value="—" />
        <MetricCard label="Reminders" value="—" />
      </div>

      {/* Charts */}
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(360px, 1fr))', gap: 'var(--space-4)' }}>
        <ChartCard
          title="Memory growth"
          subtitle="30 days"
          height={220}
          loading={engram.loading}
          isRefetching={engram.isRefetching}
          empty={!engram.loading && memoryGrowthData.length === 0}
          emptyMessage={memoryGrowthSeriesMissing ? 'backend does not expose a memory-inserts series yet' : 'No memories yet'}
          emptyHint={memoryGrowthSeriesMissing ? undefined : "they'll appear as you talk"}
          controls={<TableViewControls view={memoryGrowthTable.view} onChange={memoryGrowthTable.setView} />}
        >
          <TableView
            view={memoryGrowthTable.view}
            columns={[
              { key: 'date', header: 'Date', render: (r: { date: string; total: number }) => r.date },
              { key: 'total', header: 'Total memories', align: 'right', render: (r: { date: string; total: number }) => String(r.total) },
            ]}
            rows={memoryGrowthData}
            rowKey={(r, i) => `${r.date}-${i}`}
          >
            <ResponsiveContainer width="100%" height="100%">
              <AreaChart data={memoryGrowthData}>
                <CartesianGrid {...rechartsGridProps()} vertical={false} />
                <XAxis dataKey="date" tick={rechartsTickStyle()} tickLine={false} axisLine={false} />
                <YAxis tick={rechartsTickStyle()} tickLine={false} axisLine={false} width={40} />
                <Tooltip contentStyle={rechartsTooltipStyle()} />
                <Area
                  type="monotone"
                  dataKey="total"
                  stroke="var(--series-1)"
                  fill="var(--series-1)"
                  fillOpacity={0.1}
                  strokeWidth={2}
                />
              </AreaChart>
            </ResponsiveContainer>
          </TableView>
        </ChartCard>

        <ChartCard
          title="Routing mix"
          subtitle="14 days · fast vs deep"
          height={220}
          loading={analyticsRouting.loading}
          isRefetching={analyticsRouting.isRefetching}
          empty={!analyticsRouting.loading && dailyData.length === 0}
          emptyMessage="No routing activity yet"
          controls={<TableViewControls view={routingMixTable.view} onChange={routingMixTable.setView} />}
          footer={
            <ChartLegend
              entries={[
                { id: 'fast', label: 'fast', color: CATEGORICAL_HEX[3] },
                { id: 'deep', label: 'deep', color: CATEGORICAL_HEX[0] },
              ]}
            />
          }
        >
          <TableView
            view={routingMixTable.view}
            columns={[
              { key: 'date', header: 'Date', render: (r: { date: string; turns: number; deep: number }) => r.date },
              { key: 'fast', header: 'Fast', align: 'right', render: (r: { date: string; turns: number; deep: number }) => String(r.turns - r.deep) },
              { key: 'deep', header: 'Deep', align: 'right', render: (r: { date: string; turns: number; deep: number }) => String(r.deep) },
            ]}
            rows={dailyData}
            rowKey={(r, i) => `${r.date}-${i}`}
          >
            <ResponsiveContainer width="100%" height="100%">
              <BarChart data={dailyData.map(d => ({ ...d, fast: d.turns - d.deep }))} barGap={2}>
                <CartesianGrid {...rechartsGridProps()} vertical={false} />
                <XAxis dataKey="date" tick={rechartsTickStyle()} tickLine={false} axisLine={false} />
                <YAxis tick={rechartsTickStyle()} tickLine={false} axisLine={false} width={40} />
                <Tooltip contentStyle={rechartsTooltipStyle()} />
                <Bar dataKey="fast" stackId="turns" fill={CATEGORICAL_HEX[3]} radius={[0, 0, 0, 0]} maxBarSize={24} />
                <Bar dataKey="deep" stackId="turns" fill={CATEGORICAL_HEX[0]} radius={[4, 4, 0, 0]} maxBarSize={24} />
              </BarChart>
            </ResponsiveContainer>
          </TableView>
        </ChartCard>

        <ChartCard
          title="Top tools"
          subtitle="7 days"
          height={220}
          loading={analyticsTools.loading}
          isRefetching={analyticsTools.isRefetching}
          empty={!analyticsTools.loading && topToolsData.length === 0}
          emptyMessage="No tool calls yet"
          controls={<TableViewControls view={topToolsTable.view} onChange={topToolsTable.setView} />}
        >
          <TableView
            view={topToolsTable.view}
            columns={[
              { key: 'name', header: 'Tool', render: (r: { name: string; count: number }) => r.name },
              { key: 'count', header: 'Calls', align: 'right', render: (r: { name: string; count: number }) => String(r.count) },
            ]}
            rows={topToolsData}
            rowKey={(r, i) => `${r.name}-${i}`}
          >
            <ResponsiveContainer width="100%" height="100%">
              <BarChart data={topToolsData} layout="vertical" margin={{ left: 8, right: 24 }}>
                <CartesianGrid {...rechartsGridProps()} horizontal={false} />
                <XAxis type="number" tick={rechartsTickStyle()} tickLine={false} axisLine={false} />
                <YAxis
                  type="category"
                  dataKey="name"
                  tick={rechartsTickStyle()}
                  tickLine={false}
                  axisLine={false}
                  width={120}
                />
                <Tooltip contentStyle={rechartsTooltipStyle()} />
                <Bar dataKey="count" fill="var(--series-1)" radius={[0, 4, 4, 0]} maxBarSize={18} label={{ position: 'right', fill: 'var(--text-muted)', fontSize: 11 }} />
              </BarChart>
            </ResponsiveContainer>
          </TableView>
        </ChartCard>
      </div>

      {/* Activity feed */}
      <div style={CARD_STYLE}>
        <div style={{ fontSize: 'var(--fs-sm)', fontWeight: 600, color: 'var(--text-100)', marginBottom: 'var(--space-2)' }}>
          Activity
        </div>
        {events.loading ? (
          <ChartEmpty height={120} message="Loading…" />
        ) : eventList.length === 0 ? (
          <ChartEmpty height={120} message="No recent activity" hint="tool calls and chat turns will appear here" />
        ) : (
          <div style={{ display: 'flex', flexDirection: 'column' }}>
            {eventList.slice(0, 20).map((ev, i) => (
              <div
                key={`${ev.ts}-${i}`}
                style={{
                  display: 'flex',
                  alignItems: 'baseline',
                  gap: 'var(--space-2)',
                  padding: 'var(--space-1) 0',
                  borderBottom: i < eventList.length - 1 ? '1px solid var(--border-subtle, var(--border))' : 'none',
                  fontFamily: 'var(--font-mono)',
                  fontSize: 'var(--fs-xs)',
                }}
              >
                <span style={{ color: LEVEL_COLOR[ev.level], flexShrink: 0 }}>[{ev.level}]</span>
                <span style={{ flex: 1, minWidth: 0, color: 'var(--text-muted)' }}>{ev.text}</span>
                <span style={{ color: 'var(--text-faint, var(--text-muted))', flexShrink: 0 }}>{formatTime(ev.ts)}</span>
              </div>
            ))}
          </div>
        )}
      </div>

      {engram.data && !engram.data.store_ok && (
        <div style={{ color: 'var(--status-warning)', fontSize: 'var(--fs-xs)' }}>
          Engram store reports store_ok=false — memory tiles/charts above may be stale.
        </div>
      )}
      {engram.data && (
        <div style={{ color: 'var(--text-muted)', fontSize: 'var(--fs-xs)' }}>
          {formatBytes(engram.data.db_bytes)} on disk · {engram.data.embedded_pct.toFixed(1)}% embedded
        </div>
      )}
    </div>
  );
}
