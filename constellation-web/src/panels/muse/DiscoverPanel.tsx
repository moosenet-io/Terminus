// MGUI-04 (S129): muse.discover — guide screen 05, "request from the internet".
//
// `GET /api/discover` is PUBLIC and returns `{configured, region, items}`. On this
// deployment it returns `configured: true` with `items: []` — a trending provider IS
// set up, but no snapshot has been ingested yet.
//
// That distinction is the whole point of this panel's empty state. Three different
// situations would otherwise look identical:
//
//   configured: false          -> no provider set up; fix is configuration
//   configured: true, items:[] -> set up, but the trending worker has not run
//   degraded (non-2xx)         -> the endpoint itself is unreachable
//
// `useMuseSection` already distinguishes the third. This panel distinguishes the
// first two rather than rendering one anonymous empty box, because "nothing here"
// and "nothing has run" call for different fixes and a blank card says neither.
//
// THE REQUEST CTA IS INERT. Guide screen 05 shows a "Request →" button, but that is
// the acquisition WRITE path, which lives behind Muse's dual safety gate (MUSEM-05)
// and is out of scope for this read-only surface. It renders visibly disabled with an
// explanation instead of being omitted, so the design's shape is legible without the
// button being a lie.
import { ChartCard } from '../../viz/ChartCard';
import { useMuseDiscover, museArtUrlAt } from '../../hooks/useMuse';

const PANEL_BODY_HEIGHT = 560;

export function DiscoverPanel() {
  const { data, loading, degraded } = useMuseDiscover();

  const items = data?.items ?? [];
  // Deliberately NOT passed to ChartCard's `empty`: the standard empty state cannot
  // say WHICH of the two empties this is, and that is the useful information here.
  const showSeam = !loading && !degraded && items.length === 0;

  return (
    <ChartCard
      title="Discover"
      subtitle={data ? `beyond your library · region ${data.region}` : 'beyond your library'}
      height={PANEL_BODY_HEIGHT}
      loading={loading}
      degraded={degraded}
    >
      <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-3)', height: '100%', minHeight: 0 }}>
        {showSeam ? (
          <div
            style={{
              display: 'flex',
              flexDirection: 'column',
              gap: 'var(--space-2)',
              padding: 'var(--space-4)',
              fontSize: 'var(--fs-xs)',
              color: 'var(--text-300)',
            }}
          >
            {data?.configured ? (
              <>
                <div style={{ color: 'var(--text-100)' }}>No trending snapshot yet.</div>
                <div>
                  A trending provider <strong>is</strong> configured for region {data.region}, but no
                  snapshot has been ingested — the trending worker has not produced one. This is a
                  worker/schedule gap, not a configuration gap.
                </div>
              </>
            ) : (
              <>
                <div style={{ color: 'var(--text-100)' }}>No trending provider configured.</div>
                <div>
                  Discover needs a metadata/trending provider (TMDb) configured in Muse before it can
                  show anything beyond your library.
                </div>
              </>
            )}
          </div>
        ) : (
          <div style={{ flex: 1, minHeight: 0, overflowY: 'auto' }}>
            <div
              style={{
                display: 'grid',
                gridTemplateColumns: 'repeat(auto-fill, minmax(120px, 1fr))',
                gap: 'var(--space-3)',
              }}
            >
              {items.map((it, i) => (
                <div key={it.media_metadata_id ?? it.tmdb_id ?? i} style={{ display: 'flex', flexDirection: 'column', gap: 4, minWidth: 0 }}>
                  <div
                    style={{
                      aspectRatio: '2 / 3',
                      background: 'var(--space-600)',
                      border: '1px solid var(--border)',
                      borderRadius: 'var(--radius-sm, 4px)',
                      overflow: 'hidden',
                    }}
                  >
                    {it.media_metadata_id !== undefined && (
                      <img
                        src={museArtUrlAt('media_metadata', String(it.media_metadata_id), 160)}
                        alt=""
                        aria-hidden
                        loading="lazy"
                        style={{ width: '100%', height: '100%', objectFit: 'cover', display: 'block' }}
                        onError={e => {
                          (e.currentTarget as HTMLImageElement).style.visibility = 'hidden';
                        }}
                      />
                    )}
                  </div>
                  <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-100)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }} title={it.title}>
                    {it.title}
                  </div>
                  <button
                    disabled
                    title="Requesting is a write path and lives behind Muse's dual safety gate — not available from this read-only surface."
                    style={{
                      padding: '2px 8px',
                      fontSize: 'var(--fs-2xs, 10px)',
                      fontFamily: 'var(--font-mono)',
                      color: 'var(--text-500, rgba(255,255,255,0.28))',
                      background: 'transparent',
                      border: '1px solid var(--border)',
                      borderRadius: 'var(--radius-xs, 3px)',
                      cursor: 'default',
                    }}
                  >
                    Request →
                  </button>
                </div>
              ))}
            </div>
          </div>
        )}
      </div>
    </ChartCard>
  );
}
