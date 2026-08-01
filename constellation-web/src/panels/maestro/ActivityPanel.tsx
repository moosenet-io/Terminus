// MACT-04 (MUSE-124): the Maestro Activity panel — modelled directly on the house pattern in
// `terminus/ActivityPanel.tsx` (an `available === null | false | true` tri-state, filter chips
// built from the data's own distinct values, a paged `DataTable`, an explicit degrade card that
// names the cause instead of rendering an empty table).
//
// TWO PANES, VISIBLY TWO SOURCES — the whole point of this item (see MACT-04's spec item and
// `aggregationClient.ts`'s "TWO-PROXY RULE" comment above `LiveSession`/`HistorySession`):
//   - LIVE  — `useMuseLiveSessions()`, `source: 'muse-derived'` in H1. The pane header renders
//     that `source` verbatim (`liveSourceLabel`, `nowPlaying.ts`) so H2's flip to a real Maestro
//     push feed (`'maestro-live'`) is a visible, explained change — never a silent identity swap.
//   - HISTORY — `useMuseSessionHistory()`, `source: 'muse-history'` PERMANENTLY (Muse's own
//     ledger; unaffected by the H2 flip). BOTH panes read `source` off the resolved envelope
//     (`live.data?.source` / `history.data?.source`) — a review round (MUSE-124) caught the
//     HISTORY pane hardcoding the `'muse-history'` literal instead, which happened to render
//     correctly today but would have silently gone stale under any future change to what the
//     history endpoint actually reports. Fixed by threading `source` through as a prop, same as
//     LIVE; `ActivityPanel.test.ts` renders both panes and asserts on an UNEXPECTED source value
//     specifically so it fails if either hardcodes the literal again.
//
// H2 SWAP COST — corrected claim: `LiveSession.source` / `LiveSessionsResult.source` are typed
// as `LiveSessionSource = 'muse-derived' | 'maestro-live'` (aggregationClient.ts), so THIS PANEL
// needs zero changes for either value — `liveSourceLabel` already switches on the runtime string
// (see nowPlaying.ts, unit-tested with both literals plus an unrecognised one). What is NOT a
// one-line change is the PRODUCING side: `muse.sessions.live()`'s implementation, its mock, and
// eventually a new `maestro.*` proxy arm all still need real work to actually emit
// `'maestro-live'` from a real Maestro push feed. The type widening only guarantees the
// consuming side (this panel) doesn't have to move when that lands.
// `LiveSession` and `HistorySession` are branded, mutually non-substitutable types (see that
// doc comment for why) — this panel never merges their rows into one shape, and each pane
// degrades INDEPENDENTLY: a dead LIVE feed does not blank the HISTORY table and vice versa.
//
// Both hooks already degrade to `{available:false, detail}` on every failure (401 unprovisioned
// bearer, 404/501 not-yet-deployed, network error) — this panel is the ONLY thing responsible
// for turning that `detail` into an operator-actionable cause (`degradeCause`, nowPlaying.ts),
// most importantly the 401 case: `CONSTELLATION_MUSE_TOKEN` is unprovisioned on this deployment
// (TERM-549), so the protected session routes WILL 401 in practice. That must render as a named
// cause, never as an empty table quietly implying "nobody is watching".
import { useMemo, useState } from 'react';
import { Card, CardTitle } from '../../components/Card';
import { PanelRoot } from '../../components/PanelRoot';
import { Badge } from '../../components/Badge';
import { StatusPill } from '../../components/StatusPill';
import { ProgressBar } from '../../components/ProgressBar';
import { EmptyState } from '../../components/EmptyState';
import { SkeletonList } from '../../components/Skeleton';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import { useMuseLiveSessions, useMuseSessionHistory, museArtUrlAt } from '../../hooks/useMuse';
import type { HistorySession, LiveSession } from '../../lib/aggregationClient';
import {
  accountLabel,
  classifyDecision,
  degradeCause,
  distinctBy,
  historySourceLabel,
  isItemResolved,
  itemTitle,
  liveSourceLabel,
  progressInfo,
  statePillLabel,
  statePillState,
} from './nowPlaying';

const HISTORY_PAGE_SIZE = 20;
const HISTORY_FETCH_LIMIT = 200;

// ── LIVE pane ─────────────────────────────────────────────────────────────

export function LiveSessionCard({ session }: { session: LiveSession }) {
  const progress = progressInfo(session.view_offset_ms, session.duration_ms, session.progress_pct);
  const decision = classifyDecision(session.decision);
  const resolved = isItemResolved(session.item);
  const title = itemTitle(session.item);

  return (
    <Card variant="content" style={{ display: 'flex', gap: 'var(--space-3)', minWidth: 0 }}>
      <div
        style={{
          position: 'relative',
          width: 'var(--space-8)',
          aspectRatio: '2 / 3',
          flexShrink: 0,
          borderRadius: 'var(--radius-sm)',
          background: 'var(--space-600)',
          border: 'var(--border-width) solid var(--border)',
          overflow: 'hidden',
        }}
      >
        {resolved && session.item.media_item_id != null && (
          <img
            src={museArtUrlAt('media_item', String(session.item.media_item_id), 160)}
            alt=""
            aria-hidden
            loading="lazy"
            style={{ width: '100%', height: '100%', objectFit: 'cover', display: 'block' }}
            // A missing poster degrades to the tile's own background, never a broken-image
            // glyph (S129/MGUI-01; TERM-550 — `media_item` is a valid art KIND, `poster` is not).
            onError={e => { (e.currentTarget as HTMLImageElement).style.visibility = 'hidden'; }}
          />
        )}
      </div>

      <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-1)', minWidth: 0, flex: 1 }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', minWidth: 0 }}>
          <span
            title={title}
            style={{
              fontSize: 'var(--fs-sm)', fontWeight: 600, color: 'var(--text-primary)',
              overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', minWidth: 0,
            }}
          >
            {title}
          </span>
          {!resolved && <Badge tone="neutral">unresolved</Badge>}
        </div>

        <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)' }}>
          {accountLabel(session.account)} · {session.device ?? session.player ?? 'unknown device'}
        </div>

        {/* A `null` pct (unknown duration, MACT-01) is UNREPORTABLE, not zero — rendering
            `<ProgressBar pct={0}>` here would draw a real, empty-looking track that reads as
            "just started" to a viewer, indistinguishable from an actual 0% measurement. Review
            finding (MUSE-124): omit the bar entirely rather than fabricate that reading; the
            text label below already says so explicitly. */}
        {progress.pct != null && <ProgressBar pct={progress.pct} style={{ marginTop: 2 }} />}
        <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-faint)' }}>
          {progress.combinedLabel}{progress.pct == null ? ' (progress not reported)' : ''}
        </div>

        <div style={{ display: 'flex', gap: 'var(--space-1)', flexWrap: 'wrap', marginTop: 'var(--space-1)' }}>
          <StatusPill state={statePillState(session.state)} label={statePillLabel(session.state)} />
          <span title={decision.tooltip ?? undefined}>
            <Badge tone={decision.tone}>{decision.label}</Badge>
          </span>
        </div>
      </div>
    </Card>
  );
}

export function LivePane({ available, detail, sessions, source }: {
  available: boolean | null;
  detail: string | undefined;
  sessions: LiveSession[];
  source: string | null;
}) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)' }}>
      <CardTitle subtitle={source ? liveSourceLabel(source) : 'now playing'}>
        Live
      </CardTitle>

      {available === null && (
        <Card variant="content"><SkeletonList rows={4} /></Card>
      )}

      {available === false && (
        <Card variant="content">
          <EmptyState
            title="Live session feed is unavailable"
            message={degradeCause(detail)}
            tone="var(--flux-amber, var(--text-500))"
          />
        </Card>
      )}

      {available === true && sessions.length === 0 && (
        <Card variant="content">
          {/* A fact, visually distinct from the degrade card above (200, zero rows — not an
              error). EmptyState's neutral tone + no cause-message keeps that distinction. */}
          <EmptyState title="Nobody is watching right now" />
        </Card>
      )}

      {available === true && sessions.length > 0 && (
        <div
          style={{
            display: 'grid',
            gridTemplateColumns: 'repeat(auto-fill, minmax(20rem, 1fr))',
            gap: 'var(--space-3)',
            maxHeight: 640,
            overflowY: 'auto',
            paddingRight: 2,
          }}
        >
          {sessions.map(s => <LiveSessionCard key={s.session_key ?? s.session_id} session={s} />)}
        </div>
      )}
    </div>
  );
}

// ── HISTORY pane ──────────────────────────────────────────────────────────

type HistoryFilterKey = 'account' | 'device' | 'decision';

function historyFilterValue(row: HistorySession, key: HistoryFilterKey): string | null {
  if (key === 'account') return accountLabel(row.account);
  if (key === 'device') return row.device;
  return classifyDecision(row.decision).label;
}

export function HistoryPane({ available, detail, sessions, source }: {
  available: boolean | null;
  detail: string | undefined;
  sessions: HistorySession[];
  source: string | null;
}) {
  const [accountFilter, setAccountFilter] = useState<string | null>(null);
  const [deviceFilter, setDeviceFilter] = useState<string | null>(null);
  const [decisionFilter, setDecisionFilter] = useState<string | null>(null);
  const [page, setPage] = useState(0);

  const filtered = useMemo(() => sessions.filter(s =>
    (!accountFilter || historyFilterValue(s, 'account') === accountFilter) &&
    (!deviceFilter || historyFilterValue(s, 'device') === deviceFilter) &&
    (!decisionFilter || historyFilterValue(s, 'decision') === decisionFilter),
  ), [sessions, accountFilter, deviceFilter, decisionFilter]);

  const pageCount = Math.max(1, Math.ceil(filtered.length / HISTORY_PAGE_SIZE));
  const clampedPage = Math.min(page, pageCount - 1);
  const pageRows = filtered.slice(clampedPage * HISTORY_PAGE_SIZE, clampedPage * HISTORY_PAGE_SIZE + HISTORY_PAGE_SIZE);

  const columns: DataTableColumn<HistorySession>[] = [
    { key: 'started_at', header: 'Started', render: r => new Date(r.started_at).toLocaleString() },
    { key: 'item', header: 'Title', render: r => <span title={itemTitle(r.item)}>{itemTitle(r.item)}</span> },
    { key: 'account', header: 'Account', render: r => accountLabel(r.account) },
    { key: 'device', header: 'Device', render: r => r.device ?? '—' },
    {
      key: 'decision', header: 'Decision', render: r => {
        const d = classifyDecision(r.decision);
        return <span title={d.tooltip ?? undefined}><Badge tone={d.tone}>{d.label}</Badge></span>;
      },
    },
    { key: 'progress', header: 'Progress', render: r => progressInfo(r.view_offset_ms, r.duration_ms, r.progress_pct).combinedLabel },
  ];

  function filterRow(label: string, key: HistoryFilterKey, value: string | null, setValue: (v: string | null) => void) {
    const options = distinctBy(sessions, s => historyFilterValue(s, key));
    if (options.length === 0) return null;
    return (
      <div style={{ display: 'flex', gap: 'var(--space-1)', alignItems: 'center', flexWrap: 'wrap' }}>
        <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', marginRight: 4 }}>{label}:</span>
        <button
          type="button"
          onClick={() => { setValue(null); setPage(0); }}
          className={`h-badge ${value === null ? 'h-badge-violet' : 'h-badge-neutral'}`}
          style={{ cursor: 'pointer', border: 'none' }}
        >
          all
        </button>
        {options.map(o => (
          <button
            key={o}
            type="button"
            onClick={() => { setValue(o); setPage(0); }}
            className={`h-badge ${value === o ? 'h-badge-violet' : 'h-badge-neutral'}`}
            style={{ cursor: 'pointer', border: 'none' }}
          >
            {o}
          </button>
        ))}
      </div>
    );
  }

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)' }}>
      {/* Read from the resolved envelope, not a hardcoded literal — see the module doc's
          "BOTH panes read `source` off the resolved envelope" note (MUSE-124 review fix). */}
      <CardTitle subtitle={source ? historySourceLabel(source) : 'watch history'}>
        History
      </CardTitle>

      {available === null && (
        <Card variant="content"><SkeletonList rows={6} /></Card>
      )}

      {available === false && (
        <Card variant="content">
          <EmptyState title="Session history is unavailable" message={degradeCause(detail)} tone="var(--flux-amber, var(--text-500))" />
        </Card>
      )}

      {available === true && (
        <>
          <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)' }}>
            {filterRow('Account', 'account', accountFilter, setAccountFilter)}
            {filterRow('Device', 'device', deviceFilter, setDeviceFilter)}
            {filterRow('Decision', 'decision', decisionFilter, setDecisionFilter)}
          </div>

          <Card variant="content">
            <DataTable
              columns={columns}
              rows={pageRows}
              rowKey={(r, i) => `${r.session_key ?? r.session_id}-${i}`}
              emptyMessage={sessions.length === 0 ? 'No history recorded' : 'No sessions match this filter'}
            />
          </Card>

          {filtered.length > 0 && (
            <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', fontSize: 'var(--fs-xs)', color: 'var(--text-muted)' }}>
              <button
                type="button"
                disabled={clampedPage === 0}
                onClick={() => setPage(p => Math.max(0, p - 1))}
                style={{ opacity: clampedPage === 0 ? 0.4 : 1 }}
              >
                ← prev
              </button>
              <span>
                page {clampedPage + 1} / {pageCount} · {filtered.length} session{filtered.length === 1 ? '' : 's'}
              </span>
              <button
                type="button"
                disabled={clampedPage >= pageCount - 1}
                onClick={() => setPage(p => Math.min(pageCount - 1, p + 1))}
                style={{ opacity: clampedPage >= pageCount - 1 ? 0.4 : 1 }}
              >
                next →
              </button>
            </div>
          )}
        </>
      )}
    </div>
  );
}

// ── Panel root ────────────────────────────────────────────────────────────

export function ActivityPanel() {
  const live = useMuseLiveSessions();
  const history = useMuseSessionHistory(HISTORY_FETCH_LIMIT);

  const liveAvailable = live.loading ? null : live.degraded === false;
  const historyAvailable = history.loading ? null : history.degraded === false;

  return (
    <PanelRoot style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-5)' }}>
      <CardTitle subtitle="Who is watching what, and what the box is doing">
        Maestro — Activity
      </CardTitle>

      <LivePane
        available={liveAvailable}
        detail={live.degraded ? live.degraded.detail : undefined}
        sessions={live.data?.sessions ?? []}
        source={live.data?.source ?? null}
      />

      <HistoryPane
        available={historyAvailable}
        detail={history.degraded ? history.degraded.detail : undefined}
        sessions={history.data?.sessions ?? []}
        source={history.data?.source ?? null}
      />
    </PanelRoot>
  );
}
