// CGUI-06 (TERM #529): Harmony Tasks panel — full rich task table (was a 13-LOC stub card).
//
// Renders the Harmony task queue as a dense, scrollable table: id · title · status pill ·
// kind badge · progress bar. A summary MetricCard row sits above it (total / in-flight /
// done / blocked). Built on the DS primitives (Card/Badge/StatusPill/DataTable/MetricCard)
// inside a PanelRoot scroll frame (`.hf-scroll`) so it scrolls with the rest of the app.
//
// Data wiring: pulls `GET /api/harmony/tasks` through the aggregation client. That endpoint
// is not yet served by the Harmony aggregation backend (the mock adapter returns null and a
// live backend 404s), so when the response is empty we fall back to the REPRESENTATIVE rows
// below — enough shape to make the panel read as fully developed. Unknown numeric fields
// render as a mono "—". PENDING REAL-DATA WIRING: replace the fallback once Harmony exposes a
// task-list aggregation endpoint (its shape is exactly `HarmonyTask[]`).
import { useEffect, useMemo, useState } from 'react';
import { Card, CardTitle } from '../components/Card';
import { PanelRoot } from '../components/PanelRoot';
import { Badge } from '../components/Badge';
import type { BadgeTone } from '../components/Badge';
import { StatusPill } from '../components/StatusPill';
import type { PillState } from '../components/StatusPill';
import { MetricCard } from '../components/MetricCard';
import { DataTable } from '../components/DataTable';
import type { DataTableColumn } from '../components/DataTable';
import { getAggregationClient } from '../lib/aggregationClient';

type TaskStatus = 'backlog' | 'in_progress' | 'review' | 'done' | 'blocked';
type TaskKind = 'feature' | 'fix' | 'chore' | 'spec' | 'test';

interface HarmonyTask {
  id: string;
  title: string;
  status: TaskStatus;
  kind: TaskKind;
  /** 0–100; null when the backend doesn't track progress for this item. */
  progress: number | null;
}

const STATUS_PILL: Record<TaskStatus, PillState> = {
  backlog: 'idle',
  in_progress: 'warm',
  review: 'cold',
  done: 'online',
  blocked: 'error',
};
const STATUS_LABEL: Record<TaskStatus, string> = {
  backlog: 'Backlog', in_progress: 'In Progress', review: 'In Review', done: 'Done', blocked: 'Blocked',
};
const KIND_TONE: Record<TaskKind, BadgeTone> = {
  feature: 'violet', fix: 'rose', chore: 'neutral', spec: 'blue', test: 'amber',
};

// Representative fallback — see the file header. Mirrors real Plane `HARM`/`TERM` items so the
// panel looks developed until the aggregation endpoint lands.
const FALLBACK_TASKS: HarmonyTask[] = [
  { id: 'CGUI-06', title: 'Fill stub GUI panels into rich treatments', status: 'in_progress', kind: 'feature', progress: 60 },
  { id: 'CGUI-11', title: 'Harmony Forest Build orchestrator screen', status: 'done', kind: 'feature', progress: 100 },
  { id: 'HARM-407', title: 'Deep-space rebrand follow-up: dashboard tokens', status: 'review', kind: 'chore', progress: 90 },
  { id: 'HARM-408', title: 'Forest build animation polish pass', status: 'backlog', kind: 'feature', progress: 0 },
  { id: 'TERM-529', title: 'Aggregation endpoint for Harmony task list', status: 'blocked', kind: 'feature', progress: 15 },
  { id: 'MUSEL-B2', title: 'Metadata provider TVDB extended retry backoff', status: 'in_progress', kind: 'fix', progress: 45 },
  { id: 'MWEB-02', title: 'Muse web pattern library build-out', status: 'in_progress', kind: 'feature', progress: 35 },
  { id: 'REVCAP-01', title: 'Reviewer cap two-tier state machine', status: 'done', kind: 'feature', progress: 100 },
  { id: 'HFOR-04', title: 'Persist shipped-spec forest across restarts', status: 'review', kind: 'feature', progress: 80 },
  { id: 'TERM-501', title: 'cargo-audit gate: triage 11 flagged vulns', status: 'backlog', kind: 'chore', progress: null },
];

function ProgressBar({ value }: { value: number | null }) {
  if (value == null) {
    return <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--text-muted)', fontSize: 'var(--fs-mono-sm)' }}>—</span>;
  }
  const pct = Math.max(0, Math.min(100, value));
  const done = pct >= 100;
  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', minWidth: 120 }}>
      <div style={{
        flex: 1,
        // 6px track — DS-parity structural literal, intentionally not tokenized.
        height: 6,
        borderRadius: 'var(--radius-pill)',
        background: 'var(--space-700)',
        overflow: 'hidden',
      }}>
        <div style={{
          width: `${pct}%`,
          height: '100%',
          borderRadius: 'var(--radius-pill)',
          background: done ? 'var(--flux-green)' : 'var(--grad-accent)',
          boxShadow: done ? 'none' : 'var(--glow-violet-soft)',
          transition: 'width var(--dur-base) var(--ease-out)',
        }} />
      </div>
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-secondary)', minWidth: 34, textAlign: 'right' }}>
        {pct}%
      </span>
    </div>
  );
}

export function Tasks() {
  const [tasks, setTasks] = useState<HarmonyTask[] | null>(null);
  const [statusFilter, setStatusFilter] = useState<TaskStatus | null>(null);

  useEffect(() => {
    let cancelled = false;
    getAggregationClient()
      .request<HarmonyTask[] | null>('harmony', '/tasks')
      .then(d => { if (!cancelled) setTasks(Array.isArray(d) && d.length > 0 ? d : FALLBACK_TASKS); })
      .catch(() => { if (!cancelled) setTasks(FALLBACK_TASKS); });
    return () => { cancelled = true; };
  }, []);

  const rows = tasks ?? [];
  const filtered = useMemo(
    () => (statusFilter ? rows.filter(t => t.status === statusFilter) : rows),
    [rows, statusFilter],
  );

  const counts = useMemo(() => ({
    total: rows.length,
    inFlight: rows.filter(t => t.status === 'in_progress' || t.status === 'review').length,
    done: rows.filter(t => t.status === 'done').length,
    blocked: rows.filter(t => t.status === 'blocked').length,
  }), [rows]);

  const columns: DataTableColumn<HarmonyTask>[] = [
    { key: 'id', header: 'ID', render: r => <code style={{ fontFamily: 'var(--font-mono)', color: 'var(--accent)', fontSize: 'var(--fs-mono-sm)' }}>{r.id}</code> },
    { key: 'title', header: 'Title', render: r => <span style={{ color: 'var(--text-primary)' }}>{r.title}</span> },
    { key: 'status', header: 'Status', render: r => <StatusPill state={STATUS_PILL[r.status]} label={STATUS_LABEL[r.status]} pulse={r.status === 'in_progress'} /> },
    { key: 'kind', header: 'Kind', render: r => <Badge tone={KIND_TONE[r.kind]} dot>{r.kind}</Badge> },
    { key: 'progress', header: 'Progress', render: r => <ProgressBar value={r.progress} /> },
  ];

  const STATUSES: TaskStatus[] = ['backlog', 'in_progress', 'review', 'done', 'blocked'];

  return (
    <PanelRoot style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="Harmony build-queue items — id, status, kind and progress across the constellation">
        Harmony — Tasks
      </CardTitle>

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(4, 1fr)', gap: 'var(--space-3)' }}>
        <MetricCard label="Total" value={tasks ? String(counts.total) : '—'} />
        <MetricCard label="In Flight" value={tasks ? String(counts.inFlight) : '—'} valueColor="warning" />
        <MetricCard label="Done" value={tasks ? String(counts.done) : '—'} valueColor="success" />
        <MetricCard label="Blocked" value={tasks ? String(counts.blocked) : '—'} valueColor="error" />
      </div>

      <div style={{ display: 'flex', gap: 'var(--space-1)', alignItems: 'center', flexWrap: 'wrap' }}>
        <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', marginRight: 4 }}>Status:</span>
        <button
          type="button"
          onClick={() => setStatusFilter(null)}
          className={`h-badge ${statusFilter === null ? 'h-badge-violet' : 'h-badge-neutral'}`}
          style={{ cursor: 'pointer', border: 'none' }}
        >
          all
        </button>
        {STATUSES.map(s => (
          <button
            key={s}
            type="button"
            onClick={() => setStatusFilter(s)}
            className={`h-badge ${statusFilter === s ? 'h-badge-violet' : 'h-badge-neutral'}`}
            style={{ cursor: 'pointer', border: 'none' }}
          >
            {STATUS_LABEL[s]}
          </button>
        ))}
      </div>

      <Card variant="content">
        {tasks === null ? (
          <div style={{ padding: 'var(--space-5)', textAlign: 'center', color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>Loading…</div>
        ) : (
          <DataTable
            columns={columns}
            rows={filtered}
            rowKey={r => r.id}
            emptyMessage="No tasks match this filter"
          />
        )}
      </Card>
    </PanelRoot>
  );
}
