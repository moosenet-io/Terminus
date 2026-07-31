// MGUI-04 (S129): muse.discover — guide screen 05, "request from the internet".
//
// `GET /api/discover` is PUBLIC and returns `{configured, region, items}`. On this
// deployment: `configured: true`, `items: []`.
//
// The empty state distinguishes the two things the RESPONSE actually tells us apart,
// because they need different fixes:
//
//   configured: false          -> no provider set up      (a configuration fix)
//   configured: true, items:[] -> set up, returned nothing (something else)
//   degraded (non-2xx)         -> endpoint unreachable    (handled by useMuseSection)
//
// It deliberately does NOT name a cause for the second case. An earlier version said
// "no snapshot has been ingested — the trending worker has not produced one", and a
// reviewer correctly called that an over-claim: an empty `items` is equally
// consistent with every trending title already being in the library, or a provider
// query that simply came back empty. The endpoint reports configuration and items,
// not why the list is empty, so the panel lists the possibilities instead of
// asserting one. Naming the wrong cause sends an operator to fix the wrong thing.
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
        {/* Visible, programmatically-associated explanation for the inert CTA below. */}
        {items.length > 0 && (
          <div id="discover-request-disabled-note" style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))' }}>
            Requesting is a write path behind Muse's dual safety gate, so it is not available from
            this read-only surface.
          </div>
        )}
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
                <div style={{ color: 'var(--text-100)' }}>Nothing to discover right now.</div>
                <div>
                  A trending provider <strong>is</strong> configured for region {data.region}, and it
                  returned no titles. The endpoint reports configuration and items, not why the list
                  is empty, so this could be any of: no snapshot ingested yet, every trending title
                  already in your library, or a provider query that came back empty.
                </div>
              </>
            ) : (
              <>
                <div style={{ color: 'var(--text-100)' }}>
                  {data?.metadata_provider_only
                    ? 'Configured for metadata, but not for trending.'
                    : 'No trending provider configured.'}
                </div>
                {/* MUSE #111: render the SERVER's reason when it sends one. The endpoint now
                    distinguishes "no TMDb client at all" from "a key-less proxy that serves movie
                    metadata but has no trending endpoint" — two states needing completely
                    different operator actions. This panel printed one generic sentence telling the
                    operator to configure TMDb, which was actively misleading on this deployment:
                    TMDb IS configured, just without an API key, and no amount of configuring it
                    "in Muse" would have helped. The code that knows the fact should state it. */}
                <div>
                  {data?.reason ??
                    'Discover needs a metadata/trending provider (TMDb) configured in Muse before it can show anything beyond your library.'}
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
                  {/* Disabled controls are not keyboard-focusable and `title` is not a
                      reliable accessible explanation, so the reason is ALSO visible text
                      and associated via aria-describedby (reviewer finding). */}
                  <button
                    disabled
                    aria-describedby="discover-request-disabled-note"
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
