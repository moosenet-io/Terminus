// MGUI-07 (S129): the taste PROFILE — guide screen 07's genre-lean bars, context
// centroids, and the you-vs-the-masses divergence figures.
//
// This binds `GET /api/taste`, which is the MUSE-10/13 taste model. The existing
// TastePanel cards bind `/api/graph/*` — household analytics, a different thing —
// and both belong on this page; this adds the profile the guide actually specifies.
//
// WHAT IS REAL HERE AND WHAT IS NOT, measured from a live capture:
//
//   decade_lean   5 entries   REAL
//   divergence    adventurousness / contrarian_index / mainstream_score /
//                 guilty_pleasures[2]                     REAL
//   genre_lean    []          EMPTY
//   centroids     []          EMPTY
//
// So the genre bars and the context centroids the guide shows cannot be drawn. They
// are omitted rather than rendered as empty axes, which would read as "this household
// has no genre preferences" — a claim about the operator's taste that an empty array
// does not support.
//
// The copy DOES name a cause for these two, which is a deliberate exception to the
// state-only-what-you-observed rule applied elsewhere in this sprint. It is justified
// because the cause was established INDEPENDENTLY, by querying the database directly
// rather than by inferring it from the empty response:
//
//   select count(*) from genres                  -> 0
//   select count(*) from media_metadata_genres   -> 0     (MUSE #90)
//   select count(*) from personas                -> 0
//   select count(*) from embeddings              -> 0
//   select count(*) from taste_context_centroids -> 0     (MUSE #88)
//
// An empty array alone would not license those statements; a direct count does.
//
// But the RENDERING is still driven by the response, never by those counts: the
// counts were a point-in-time measurement, so hardcoding "none" would make this panel
// lie the moment either pipeline runs. The counts justify the explanatory sentence
// shown WHILE the arrays are empty — nothing more.
import { useMemo } from 'react';
import { useMuseTasteProfile } from '../../hooks/useMuse';
import { ChartCard } from '../../viz/ChartCard';
import { RadarChartKit } from '../../viz/RadarChart';

function Bar({ label, value, max }: { label: string; value: number; max: number }) {
  const pct = max > 0 ? Math.max(0, Math.min(100, (value / max) * 100)) : 0;
  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)' }}>
      <span style={{ minWidth: 56, fontSize: 'var(--fs-2xs, 10px)', fontFamily: 'var(--font-mono)', color: 'var(--text-300)' }}>
        {label}
      </span>
      <span style={{ flex: 1, height: 8, background: 'var(--space-600)', borderRadius: 2, overflow: 'hidden' }}>
        <span style={{ display: 'block', width: `${pct}%`, height: '100%', background: 'var(--accent, #8b5cf6)' }} />
      </span>
      <span style={{ minWidth: 40, textAlign: 'right', fontSize: 'var(--fs-2xs, 10px)', fontFamily: 'var(--font-mono)', color: 'var(--text-200)', fontVariantNumeric: 'tabular-nums' }}>
        {value.toFixed(2)}
      </span>
    </div>
  );
}

function Stat({ label, value }: { label: string; value: number | undefined }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 2 }}>
      <span style={{ fontSize: 'var(--fs-2xs, 10px)', textTransform: 'uppercase', letterSpacing: '0.04em', color: 'var(--text-400, var(--text-300))' }}>
        {label}
      </span>
      {/* An absent metric shows an em-dash. Rendering 0 would be a claim — e.g.
          mainstream_score 0 means "maximally contrarian", not "unknown". */}
      <span style={{ fontSize: 'var(--fs-sm, 13px)', fontFamily: 'var(--font-mono)', color: 'var(--text-100)', fontVariantNumeric: 'tabular-nums' }}>
        {typeof value === 'number' ? value.toFixed(2) : '—'}
      </span>
    </div>
  );
}

function Missing({ what }: { what: string }) {
  return (
    <div style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))', fontStyle: 'italic' }}>
      {what}
    </div>
  );
}

export function TasteProfile() {
  const { data, loading, degraded } = useMuseTasteProfile();

  const decades = data?.decade_lean ?? [];
  const genres = data?.genre_lean ?? [];
  const div = data?.divergence ?? null;
  const centroids = data?.centroids ?? [];
  const maxDecade = decades.reduce((m, d) => Math.max(m, d.weight), 0);
  const maxGenre = genres.reduce((m, g) => Math.max(m, g.weight), 0);

  // All-or-nothing: `RadarChartKit` needs equal vertex counts, and more importantly a
  // radar missing an axis reads as a DIFFERENT shape rather than an incomplete one.
  // Values are clamped to the component's documented 0..1 domain here (it explicitly
  // does not re-clamp — an out-of-domain value is a caller bug).
  const radarAxes = useMemo(() => {
    const a = div?.adventurousness;
    const c = div?.contrarian_index;
    const m = div?.mainstream_score;
    if (typeof a !== 'number' || typeof c !== 'number' || typeof m !== 'number') return null;
    const clamp = (v: number) => Math.max(0, Math.min(1, v));
    return [
      { axis: 'adventurous', value: clamp(a) },
      { axis: 'contrarian', value: clamp(c) },
      { axis: 'mainstream', value: clamp(m) },
    ];
  }, [div]);

  // `has_data: false` means the taste model has never been computed for this account —
  // materially different from "computed and found nothing".
  const neverComputed = !loading && !degraded && data !== null && data.has_data === false;

  return (
    <ChartCard
      title="Taste profile"
      subtitle={div?.computed_at ? `last computed ${new Date(div.computed_at).toLocaleDateString()}` : 'genre + decade lean, divergence'}
      height={340}
      loading={loading}
      degraded={degraded}
      empty={neverComputed}
      emptyMessage="Taste model not computed yet"
      emptyHint="taste_model.recompute has not run for this account"
    >
      <div style={{ display: 'flex', gap: 'var(--space-4)', height: '100%', minHeight: 0, overflowY: 'auto' }}>
        <div style={{ flex: 1, minWidth: 0, display: 'flex', flexDirection: 'column', gap: 'var(--space-3)' }}>
          <div>
            <div style={{ fontSize: 'var(--fs-2xs, 10px)', textTransform: 'uppercase', letterSpacing: '0.04em', color: 'var(--text-400, var(--text-300))', marginBottom: 4 }}>
              Decade lean
            </div>
            {decades.length ? (
              <div style={{ display: 'flex', flexDirection: 'column', gap: 3 }}>
                {decades.map(d => (
                  <Bar key={d.decade} label={`${d.decade}s`} value={d.weight} max={maxDecade} />
                ))}
              </div>
            ) : (
              <Missing what="No decade lean computed." />
            )}
          </div>

          <div>
            <div style={{ fontSize: 'var(--fs-2xs, 10px)', textTransform: 'uppercase', letterSpacing: '0.04em', color: 'var(--text-400, var(--text-300))', marginBottom: 4 }}>
              Genre lean
            </div>
            {genres.length ? (
              <div style={{ display: 'flex', flexDirection: 'column', gap: 3 }}>
                {genres.map(g => (
                  <Bar key={g.genre} label={g.genre} value={g.weight} max={maxGenre} />
                ))}
              </div>
            ) : (
              <Missing what="No genre lean returned. The genre tables were empty when last checked directly (genres and media_metadata_genres both 0 rows), so no genre preference can be derived — MUSE #90." />
            )}
          </div>
        </div>

        <div style={{ flex: '0 0 240px', display: 'flex', flexDirection: 'column', gap: 'var(--space-3)' }}>
          <div>
            <div style={{ fontSize: 'var(--fs-2xs, 10px)', textTransform: 'uppercase', letterSpacing: '0.04em', color: 'var(--text-400, var(--text-300))', marginBottom: 6 }}>
              You vs the masses
            </div>
            {div ? (
              <>
                {/* The guide's divergence RADAR. Rendered only when all three axes are
                    present: a radar with a missing vertex is not a partial radar, it is
                    a differently-shaped one, and the shape is the whole message. When an
                    axis is absent the scalars below still show exactly what is known. */}
                {radarAxes ? (
                  <RadarChartKit data={radarAxes} max={1} height={150} primaryLabel="you" />
                ) : (
                  <Missing what="Radar needs all three divergence axes; some are absent, so the figures are shown as values only." />
                )}
                <div style={{ display: 'flex', gap: 'var(--space-3)', flexWrap: 'wrap', marginTop: 'var(--space-2)' }}>
                  <Stat label="adventurous" value={div.adventurousness} />
                  <Stat label="contrarian" value={div.contrarian_index} />
                  <Stat label="mainstream" value={div.mainstream_score} />
                </div>
              </>
            ) : (
              <Missing what="No divergence computed." />
            )}
          </div>

          {div?.guilty_pleasures && div.guilty_pleasures.length > 0 && (
            <div>
              <div style={{ fontSize: 'var(--fs-2xs, 10px)', textTransform: 'uppercase', letterSpacing: '0.04em', color: 'var(--text-400, var(--text-300))', marginBottom: 4 }}>
                Guilty pleasures
              </div>
              {div.guilty_pleasures.map(g => (
                <div key={g.media_metadata_id} style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-100)' }}>
                  {g.title}
                  <span style={{ color: 'var(--text-300)', fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-2xs, 10px)' }}>
                    {' '}×{g.rewatch_count}
                  </span>
                </div>
              ))}
            </div>
          )}

          <div>
            <div style={{ fontSize: 'var(--fs-2xs, 10px)', textTransform: 'uppercase', letterSpacing: '0.04em', color: 'var(--text-400, var(--text-300))', marginBottom: 4 }}>
              Context centroids
            </div>
            {/* Driven by the RESPONSE, not by the zero-row counts. Those counts were a
                point-in-time measurement; they do not make emptiness a permanent
                invariant, and hardcoding "None" would make this panel start LYING the
                moment the embedding pipeline runs (codex). The counts only justify the
                explanatory sentence shown while it is in fact empty. */}
            {centroids.length > 0 ? (
              <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-100)', fontFamily: 'var(--font-mono)' }}>
                {centroids.length} centroid{centroids.length === 1 ? '' : 's'}
              </div>
            ) : (
              <Missing what="None returned. The embedding tables were empty when last checked directly (personas, embeddings and taste_context_centroids all 0 rows) — MUSE #88." />
            )}
          </div>
        </div>
      </div>
    </ChartCard>
  );
}
