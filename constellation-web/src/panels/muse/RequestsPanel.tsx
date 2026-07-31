// MGUI-09 + MGUI-14 (S129): curation recommendations (guide screen 08) and the
// wanted / download queue (guide screen 16), on one Requests surface.
//
// Both endpoints return 200 and are EMPTY on this deployment:
//   /api/curation        -> {account_id: 1, recommendations: []}
//   /api/requests/queue  -> {wanted: [], queue: []}
//
// So both sections render their empty state today. That is the correct outcome and
// the panels are still worth shipping: the moment curation runs or a title is
// monitored, they populate with no further work. What they must NOT do is invent
// filler to look alive — which is the entire failure mode this sprint exists to end.
//
// TWO RULES CARRIED FROM THE SPEC:
//
//  1. RATIONALE IS RENDERED VERBATIM. The guide's "rationale copy" is grounded
//     narration composed SERVER-SIDE from real facts about the title. Paraphrasing,
//     truncating mid-sentence, or prettifying it client-side would turn a grounded
//     statement into an ungrounded one — the panel would be putting words in the
//     recommender's mouth.
//
//  2. NOTHING HERE CAN TRIGGER A GRAB. This is a read surface over the acquisition
//     pipeline; the write path stays behind Muse's dual safety gate (MUSEM-05).
import { ChartCard } from '../../viz/ChartCard';
import {
  useMuseCuration,
  useMuseRequestsQueue,
  type MuseCurationItem,
  type MuseQueueRow,
} from '../../hooks/useMuse';

/** Bytes → compact figure; em-dash for absent rather than a fabricated 0 B. */
function formatSize(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined || bytes === 0) return '—';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let v = bytes;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u += 1;
  }
  return `${v >= 10 || u === 0 ? Math.round(v) : v.toFixed(1)} ${units[u]}`;
}

function formatEta(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined || seconds < 0) return '—';
  if (seconds < 60) return `${Math.round(seconds)}s`;
  if (seconds < 3600) return `${Math.round(seconds / 60)}m`;
  return `${(seconds / 3600).toFixed(1)}h`;
}

/** The recommender's own score, under whichever key it used. Returns null when none
 *  is present — a missing score renders as nothing, never as 0 (which would read as
 *  "we scored this and it scored zero"). */
function fitOf(r: MuseCurationItem): number | null {
  for (const v of [r.fit, r.taste_fit, r.score]) if (typeof v === 'number') return v;
  return null;
}

function CurationSection() {
  const { data, loading, degraded } = useMuseCuration();
  const rows = data?.recommendations ?? [];
  const empty = !loading && !degraded && rows.length === 0;

  return (
    <ChartCard
      title="Curation"
      subtitle="availability-aware · ranked"
      height={260}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No recommendations yet"
      emptyHint="Muse returned an empty recommendation set for this account"
    >
      <div style={{ height: '100%', overflowY: 'auto' }}>
        {rows.map((r, i) => {
          const fit = fitOf(r);
          // Whichever narration field the payload actually carries.
          const reason = r.reason ?? r.rationale ?? null;
          return (
            <div
              key={r.media_item_id ?? r.media_metadata_id ?? i}
              style={{
                display: 'grid',
                gridTemplateColumns: '1fr auto',
                gap: 'var(--space-2)',
                padding: '5px 0',
                borderBottom: '1px solid var(--border-subtle, rgba(255,255,255,0.05))',
              }}
            >
              <div style={{ minWidth: 0 }}>
                <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-100)' }}>
                  {r.title}
                  {r.kind && (
                    <span style={{ color: 'var(--text-300)', fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-2xs, 10px)' }}>
                      {' '}
                      {r.kind}
                    </span>
                  )}
                </div>
                {/* VERBATIM — see the module doc. Not truncated, not reworded. */}
                {reason && (
                  <div style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-300)', lineHeight: 1.45 }}>{reason}</div>
                )}
              </div>
              <div style={{ textAlign: 'right', whiteSpace: 'nowrap' }}>
                {(r.tag ?? r.source) && (
                  <div style={{ fontSize: 'var(--fs-2xs, 10px)', fontFamily: 'var(--font-mono)', color: 'var(--info, #60a5fa)' }}>
                    {r.tag ?? r.source}
                  </div>
                )}
                {fit !== null && (
                  <div style={{ fontSize: 'var(--fs-xs)', fontFamily: 'var(--font-mono)', color: 'var(--text-100)', fontVariantNumeric: 'tabular-nums' }}>
                    {fit.toFixed(2)}
                    <span style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))' }}> FIT</span>
                  </div>
                )}
              </div>
            </div>
          );
        })}
      </div>
    </ChartCard>
  );
}

function QueueRow({ q }: { q: MuseQueueRow }) {
  // Progress is only drawn when the payload actually carries it. A default 0% bar
  // would assert "this download has made no progress", which is a different claim
  // from "we don't know how far along it is".
  const pct = typeof q.progress === 'number' ? Math.max(0, Math.min(100, q.progress)) : null;
  return (
    <div style={{ padding: '5px 0', borderBottom: '1px solid var(--border-subtle, rgba(255,255,255,0.05))' }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', gap: 'var(--space-2)' }}>
        <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-100)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
          {q.title}
        </span>
        <span style={{ fontSize: 'var(--fs-2xs, 10px)', fontFamily: 'var(--font-mono)', color: 'var(--text-300)', whiteSpace: 'nowrap' }}>
          {pct !== null ? `${pct.toFixed(0)}%` : '—'} · {formatSize(q.size_bytes)} · eta {formatEta(q.eta_seconds)}
        </span>
      </div>
      {pct !== null && (
        <div style={{ height: 4, background: 'var(--space-600)', borderRadius: 2, overflow: 'hidden', marginTop: 3 }}>
          <div style={{ width: `${pct}%`, height: '100%', background: 'var(--accent, #8b5cf6)' }} />
        </div>
      )}
      {(q.client || q.status) && (
        <div style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))', fontFamily: 'var(--font-mono)' }}>
          {[q.client, q.status].filter(Boolean).join(' · ')}
        </div>
      )}
    </div>
  );
}

function WantedQueueSection() {
  const { data, loading, degraded } = useMuseRequestsQueue();
  const wanted = data?.wanted ?? [];
  const queue = data?.queue ?? [];
  const empty = !loading && !degraded && wanted.length === 0 && queue.length === 0;

  return (
    <ChartCard
      title="Wanted & download queue"
      subtitle={data ? `${wanted.length} monitored · ${queue.length} downloading` : 'wanted worker · maintenance chain'}
      height={300}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="Nothing wanted or downloading"
      emptyHint="No monitored titles are missing and the download queue is empty"
    >
      <div style={{ height: '100%', overflowY: 'auto', display: 'flex', flexDirection: 'column', gap: 'var(--space-3)' }}>
        {wanted.length > 0 && (
          <div>
            <div style={{ fontSize: 'var(--fs-2xs, 10px)', textTransform: 'uppercase', letterSpacing: '0.04em', color: 'var(--text-400, var(--text-300))', marginBottom: 4 }}>
              Monitored · wanted
            </div>
            {wanted.map((w, i) => (
              <div
                key={w.monitored_item_id ?? w.media_metadata_id ?? i}
                style={{ display: 'flex', justifyContent: 'space-between', gap: 'var(--space-2)', padding: '4px 0', borderBottom: '1px solid var(--border-subtle, rgba(255,255,255,0.05))' }}
              >
                <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-100)' }}>
                  {w.title}
                  {w.year ? <span style={{ color: 'var(--text-300)' }}> {w.year}</span> : null}
                </span>
                <span style={{ fontSize: 'var(--fs-2xs, 10px)', fontFamily: 'var(--font-mono)', color: 'var(--text-300)', whiteSpace: 'nowrap' }}>
                  {[w.kind, w.quality_profile_name ?? undefined, w.status, w.note].filter(Boolean).join(' · ') || '—'}
                </span>
              </div>
            ))}
          </div>
        )}

        {queue.length > 0 && (
          <div>
            <div style={{ fontSize: 'var(--fs-2xs, 10px)', textTransform: 'uppercase', letterSpacing: '0.04em', color: 'var(--text-400, var(--text-300))', marginBottom: 4 }}>
              Download queue
            </div>
            {queue.map((q, i) => (
              <QueueRow key={q.id ?? i} q={q} />
            ))}
          </div>
        )}
      </div>
    </ChartCard>
  );
}

export function RequestsPanel() {
  return (
    <div style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CurationSection />
      <WantedQueueSection />
    </div>
  );
}
