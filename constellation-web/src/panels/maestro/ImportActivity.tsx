// MACT-05 (MUSE-125): "What is Muse importing right now" — the Import/acquisition section of
// the Maestro Activity panel (composed into ActivityPanel.tsx below the LIVE/HISTORY panes).
//
// SURFACES EXISTING PIPELINE STATE. INVENTS NO NEW TRACKING. `GET /api/requests/queue`
// (protected — Muse `src/web/dashboard.rs::get_requests_queue`) already returns exactly this:
// `wanted[]` (monitored, no file) plus `queue[]` across the real statuses `queued` /
// `downloading` / `completed` / `importing`, each row carrying release title, indexer,
// protocol, status, size and `added_at`. `useMuseImportActivity` (useMuse.ts) is a thin,
// authoritatively-typed extension of the SAME hook `useMuseDownloadQueue` (MGUI-09/14) already
// binds — no second endpoint.
//
// THE SEAM THIS SECTION MUST RENDER HONESTLY: that handler emits `"progress": Value::Null`
// behind an in-code `// SEAM: real download %/ETA not persisted` comment. qBittorrent
// per-torrent progress genuinely is not persisted today. `importProgressDisplay`
// (importActivity.ts) renders that as "not tracked" + a tooltip naming the seam — this is the
// same failure mode MACT-04 was corrected for (`pct ?? 0` fabricating a 0% measurement); this
// section never draws a bar at an invented percentage and never silently drops the column.
//
// The presentational core (`ImportActivitySection`) takes plain props, same pattern as
// `LivePane`/`HistoryPane` in ActivityPanel.tsx — testable via `renderToStaticMarkup` with no
// hooks/fetch mocking involved. `ImportActivity` (the default export used by the panel) is the
// thin hook-wired wrapper.
import { Link } from 'react-router-dom';
import { Card, CardTitle } from '../../components/Card';
import { Badge } from '../../components/Badge';
import { EmptyState } from '../../components/EmptyState';
import { SkeletonList } from '../../components/Skeleton';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import { useMuseImportActivity, useMuseSubsystems } from '../../hooks/useMuse';
import type { MuseDownloadQueueRow, MuseWantedTitleRow } from '../../hooks/useMuse';
import { degradeCause } from './nowPlaying';
import {
  emptyQueueReason,
  formatQueueAge,
  formatQueueSize,
  groupQueueByPipelineStatus,
  importProgressDisplay,
  statusGroupLabel,
  wantedCountLabel,
  wiringDisplay,
} from './importActivity';

const IMPORT_FEED_LABEL = 'Muse import/acquisition feed';

const columns: DataTableColumn<MuseDownloadQueueRow>[] = [
  { key: 'release_title', header: 'Release', render: r => <span title={r.release_title}>{r.release_title}</span> },
  { key: 'indexer', header: 'Indexer', render: r => r.indexer ?? '—' },
  { key: 'protocol', header: 'Protocol', render: r => r.protocol ?? '—' },
  { key: 'size', header: 'Size', align: 'right', render: r => formatQueueSize(r.size_bytes) },
  { key: 'age', header: 'Age', align: 'right', render: r => formatQueueAge(r.added_at) },
  {
    key: 'progress',
    header: 'Progress',
    align: 'right',
    render: r => {
      const p = importProgressDisplay(r.progress);
      return (
        <span title={p.tooltip ?? undefined}>
          <Badge tone={p.tone}>{p.label}</Badge>
        </span>
      );
    },
  },
];

function WiringChip({ label, state }: { label: string; state: string | null }) {
  const w = wiringDisplay(state);
  return (
    <div style={{ display: 'flex', gap: 'var(--space-1)', alignItems: 'center' }}>
      <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)' }}>{label}:</span>
      <Badge tone={w.tone}>{w.label}</Badge>
    </div>
  );
}

export interface ImportActivitySectionProps {
  available: boolean | null;
  detail: string | undefined;
  wanted: MuseWantedTitleRow[];
  queue: MuseDownloadQueueRow[];
  /** The `acquisition` entry's `state` from `GET /api/subsystems`, or `null` when that section
   *  hasn't resolved yet / the key wasn't found — reused, never re-derived client-side. */
  acquisitionState: string | null;
  /** The `library_scan` entry's `state` from the same payload — surfaced alongside acquisition
   *  since import ultimately lands in the scanned library. */
  libraryScanState: string | null;
}

export function ImportActivitySection({
  available,
  detail,
  wanted,
  queue,
  acquisitionState,
  libraryScanState,
}: ImportActivitySectionProps) {
  const groups = groupQueueByPipelineStatus(queue);

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)' }}>
      <CardTitle subtitle="What Muse is grabbing and importing right now">Import activity</CardTitle>

      <div style={{ display: 'flex', gap: 'var(--space-4)', flexWrap: 'wrap', alignItems: 'center' }}>
        <WiringChip label="Acquisition" state={acquisitionState} />
        <WiringChip label="Library scan / import" state={libraryScanState} />
        {/* MGUI-14 already owns the full wanted list — this is a count + link, never a
            duplicate of that panel. */}
        <Link to="/muse/requests" style={{ fontSize: 'var(--fs-xs)', color: 'var(--accent-bright, var(--text-primary))' }}>
          {wantedCountLabel(wanted)} →
        </Link>
      </div>

      {available === null && (
        <Card variant="content"><SkeletonList rows={4} /></Card>
      )}

      {available === false && (
        <Card variant="content">
          <EmptyState
            title="Import activity is unavailable"
            message={degradeCause(detail, IMPORT_FEED_LABEL)}
            tone="var(--flux-amber, var(--text-500))"
          />
        </Card>
      )}

      {available === true && groups.length === 0 && (
        <Card variant="content">
          {/* A fact (200, zero active rows) grounded in the wanted count AND the acquisition
              subsystem's own state word (never a derived diagnosis of which dependency is
              missing) — never a bare "empty", per this item's edge case on distinguishing
              "nothing monitored" from "monitored, but the pipeline isn't producing rows". */}
          <EmptyState title="Nothing in the acquisition pipeline" message={emptyQueueReason(acquisitionState, wanted.length)} />
        </Card>
      )}

      {available === true && groups.length > 0 && (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-3)' }}>
          {groups.map(g => (
            <div key={g.status} style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-1)' }}>
              <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', fontWeight: 600 }}>
                {statusGroupLabel(g)} · {g.rows.length}
              </div>
              <Card variant="content">
                <DataTable
                  columns={columns}
                  rows={g.rows}
                  rowKey={r => String(r.id)}
                  emptyMessage="No rows"
                />
              </Card>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/** Hook-wired wrapper composed into `ActivityPanel`. Reads the acquisition + library_scan
 *  wiring state off the SAME `/api/subsystems` payload `SubsystemHealth.tsx` (MGUI-06) already
 *  renders — a second, independent fetch of that endpoint (per-section degradation is the
 *  house convention every `useMuse*` hook follows), not a new one. */
export function ImportActivity() {
  const queueSection = useMuseImportActivity();
  const subsystems = useMuseSubsystems();

  const available = queueSection.loading ? null : queueSection.degraded === false;
  const acquisitionState = subsystems.data?.subsystems.find(s => s.key === 'acquisition')?.state ?? null;
  const libraryScanState = subsystems.data?.subsystems.find(s => s.key === 'library_scan')?.state ?? null;

  return (
    <ImportActivitySection
      available={available}
      detail={queueSection.degraded ? queueSection.degraded.detail : undefined}
      wanted={queueSection.data?.wanted ?? []}
      queue={queueSection.data?.queue ?? []}
      acquisitionState={acquisitionState}
      libraryScanState={libraryScanState}
    />
  );
}
