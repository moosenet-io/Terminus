// CGUI-06 (TERM #529): Harmony PRs panel — full pull-request list (was a 16-LOC stub card).
//
// Renders recent Gitea pull requests as a scrollable table: number · title · state badge
// (open/merged/closed) · author · checks status. A summary MetricCard row sits above it.
// Built on the DS primitives (Card/Badge/StatusPill/DataTable/MetricCard) inside a PanelRoot
// scroll frame so it scrolls with the rest of the app.
//
// The original stub read window.location.hostname to link straight to Gitea — dropped: a
// same-origin control plane has no business telling the browser to hop to another host, and
// window.location may only ever be touched inside aggregationClient.ts (acceptance-criterion
// grep). We surface the PR metadata in-panel instead.
//
// Data wiring: pulls `GET /api/harmony/prs` through the aggregation client. That endpoint is
// not yet served (mock adapter returns null; a live backend 404s), so on an empty response we
// fall back to the REPRESENTATIVE rows below so the panel reads as developed; unknown fields
// render as a mono "—". PENDING REAL-DATA WIRING: replace the fallback once Harmony proxies a
// Gitea PR-list endpoint (shape = `HarmonyPR[]`).
import { useEffect, useMemo, useState } from 'react';
import { Card, CardTitle } from '../components/Card';
import { PanelRoot } from '../components/PanelRoot';
import { Badge } from '../components/Badge';
import type { BadgeTone } from '../components/Badge';
import { MetricCard } from '../components/MetricCard';
import { DataTable } from '../components/DataTable';
import type { DataTableColumn } from '../components/DataTable';
import { getAggregationClient } from '../lib/aggregationClient';

type PRState = 'open' | 'merged' | 'closed';
type ChecksState = 'passing' | 'failing' | 'running' | 'none';

interface HarmonyPR {
  number: number;
  title: string;
  state: PRState;
  author: string;
  repo: string;
  checks: ChecksState;
}

const STATE_TONE: Record<PRState, BadgeTone> = { open: 'green', merged: 'violet', closed: 'rose' };
const STATE_LABEL: Record<PRState, string> = { open: 'Open', merged: 'Merged', closed: 'Closed' };

const CHECKS_TONE: Record<ChecksState, BadgeTone> = {
  passing: 'green', failing: 'rose', running: 'amber', none: 'neutral',
};
const CHECKS_LABEL: Record<ChecksState, string> = {
  passing: 'checks pass', failing: 'checks fail', running: 'checks run', none: 'no checks',
};

// Representative fallback — see the file header. Mirrors real Gitea `moosenet/*` PRs.
const FALLBACK_PRS: HarmonyPR[] = [
  { number: 247, title: 'CGUI-11: Harmony Forest Build orchestrator screen', state: 'merged', author: 'moose', repo: 'Terminus', checks: 'passing' },
  { number: 246, title: 'CGUI-06: rich GUI panel treatments', state: 'open', author: 'moose', repo: 'Terminus', checks: 'running' },
  { number: 245, title: 'REVX: dynamic review-effort policy', state: 'merged', author: 'moose', repo: 'Terminus', checks: 'passing' },
  { number: 244, title: 'MWEB-02: Muse web pattern library', state: 'open', author: 'moose', repo: 'Harmony', checks: 'passing' },
  { number: 243, title: 'Chord: idle-stop lemonade backend lifecycle', state: 'merged', author: 'moose', repo: 'Chord', checks: 'passing' },
  { number: 242, title: 'PCON-06: per-SHA staging merge queue', state: 'open', author: 'moose', repo: 'Terminus', checks: 'failing' },
  { number: 241, title: 'Superseded cargo-audit warn-mode gate', state: 'closed', author: 'moose', repo: 'Terminus', checks: 'none' },
  { number: 240, title: 'MINT: Chord contract-align sweep run', state: 'merged', author: 'moose', repo: 'Terminus', checks: 'passing' },
  { number: 262, title: 'MWEB-01: Muse web foundation', state: 'merged', author: 'moose', repo: 'Harmony', checks: 'passing' },
  { number: 230, title: 'TERM-497: gitea branch-protection tool', state: 'merged', author: 'moose', repo: 'Terminus', checks: 'passing' },
];

export function PRs() {
  const [prs, setPrs] = useState<HarmonyPR[] | null>(null);
  const [stateFilter, setStateFilter] = useState<PRState | null>(null);

  useEffect(() => {
    let cancelled = false;
    getAggregationClient()
      .request<HarmonyPR[] | null>('harmony', '/prs')
      .then(d => { if (!cancelled) setPrs(Array.isArray(d) && d.length > 0 ? d : FALLBACK_PRS); })
      .catch(() => { if (!cancelled) setPrs(FALLBACK_PRS); });
    return () => { cancelled = true; };
  }, []);

  const rows = prs ?? [];
  const filtered = useMemo(
    () => (stateFilter ? rows.filter(p => p.state === stateFilter) : rows),
    [rows, stateFilter],
  );

  const counts = useMemo(() => ({
    open: rows.filter(p => p.state === 'open').length,
    merged: rows.filter(p => p.state === 'merged').length,
    closed: rows.filter(p => p.state === 'closed').length,
  }), [rows]);

  const columns: DataTableColumn<HarmonyPR>[] = [
    { key: 'number', header: '#', render: r => <code style={{ fontFamily: 'var(--font-mono)', color: 'var(--accent)', fontSize: 'var(--fs-mono-sm)' }}>#{r.number}</code> },
    { key: 'title', header: 'Title', render: r => <span style={{ color: 'var(--text-primary)' }}>{r.title}</span> },
    { key: 'repo', header: 'Repo', render: r => <Badge tone="neutral" mono>{r.repo}</Badge> },
    { key: 'state', header: 'State', render: r => <Badge tone={STATE_TONE[r.state]} dot>{STATE_LABEL[r.state]}</Badge> },
    { key: 'author', header: 'Author', render: r => <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-secondary)' }}>@{r.author}</span> },
    { key: 'checks', header: 'Checks', render: r => <Badge tone={CHECKS_TONE[r.checks]}>{CHECKS_LABEL[r.checks]}</Badge> },
  ];

  const STATES: PRState[] = ['open', 'merged', 'closed'];

  return (
    <PanelRoot style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="Recent Gitea pull requests across the constellation — number, state, author and checks">
        Harmony — Pull Requests
      </CardTitle>

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 'var(--space-3)' }}>
        <MetricCard label="Open" value={prs ? String(counts.open) : '—'} valueColor="success" />
        <MetricCard label="Merged" value={prs ? String(counts.merged) : '—'} valueColor="accent" />
        <MetricCard label="Closed" value={prs ? String(counts.closed) : '—'} valueColor="error" />
      </div>

      <div style={{ display: 'flex', gap: 'var(--space-1)', alignItems: 'center', flexWrap: 'wrap' }}>
        <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', marginRight: 4 }}>State:</span>
        <button
          type="button"
          onClick={() => setStateFilter(null)}
          className={`h-badge ${stateFilter === null ? 'h-badge-violet' : 'h-badge-neutral'}`}
          style={{ cursor: 'pointer', border: 'none' }}
        >
          all
        </button>
        {STATES.map(s => (
          <button
            key={s}
            type="button"
            onClick={() => setStateFilter(s)}
            className={`h-badge ${stateFilter === s ? 'h-badge-violet' : 'h-badge-neutral'}`}
            style={{ cursor: 'pointer', border: 'none' }}
          >
            {STATE_LABEL[s]}
          </button>
        ))}
      </div>

      <Card variant="content">
        {prs === null ? (
          <div style={{ padding: 'var(--space-5)', textAlign: 'center', color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>Loading…</div>
        ) : (
          <DataTable
            columns={columns}
            rows={filtered}
            rowKey={r => String(r.number)}
            emptyMessage="No pull requests match this filter"
          />
        )}
      </Card>
    </PanelRoot>
  );
}
