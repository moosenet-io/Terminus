// MGUI-01 (S129): muse.library — the poster wall. Guide screen 02.
//
// This is the panel the operator was actually asking for ("when do I get the scrollable list of
// my media"). It exists because the Muse read API had it all along: `GET /api/library` is on
// Muse's PUBLIC router and returns 1892 owned / 1629 on-disk titles with poster paths, so this
// populates with NO upstream credential — unlike the per-account sections (On Deck, Premieres,
// Taste) which stay dark until `CONSTELLATION_MUSE_TOKEN` is provisioned. Nothing here depends
// on that token.
//
// Two deliberate constraints, both learned the hard way:
//
// 1. **Art URLs go through `museArtUrl('media_metadata', id)`.** Muse's art resolver keys on
//    ENTITY KIND — it accepts `media_metadata` and `media_item` and returns a placeholder for
//    anything else. `poster` is a *variant*, not a kind; passing it was TERM #550, where every
//    on-deck poster silently rendered as a placeholder. The API's own `poster_url` field is
//    Muse-relative (`/art/media_metadata/1225`) and is deliberately NOT used as an `<img src>`:
//    the browser needs the same-origin proxy prefix that `museArtUrl` adds.
//
// 2. **The grid scrolls in its OWN container.** The page body must never become the scroller —
//    a full-page scroll on a 1892-tile wall drags the whole shell (rail, global bar) with it.
//
// KNOWN COST, measured not assumed (MUSE #100): Muse's art endpoint has no thumbnail variant, so
// each ~112px tile pulls a FULL-SIZE poster — 1.9 MB for The Martian, 780 KB for another sampled
// title. With `loading="lazy"` only visible tiles fetch, but a scroll through the wall is heavy:
// in the verification harness 103 of 240 tiles had decoded after 3s and 170 after 20s, so a first
// visit briefly shows empty tiles that fill in. That is a slow image, NOT a broken one (the art
// endpoint returns 200 with a valid JPEG — verified directly), and NOT something the panel can fix
// client-side. The real fix is a server-side thumbnail variant, tracked as MUSE #100.
import { useMemo, useState } from 'react';
import { ChartCard } from '../../viz/ChartCard';
import { useMuseLibrary, museArtUrl, type MuseLibraryItem } from '../../hooks/useMuse';

/** How many titles to request. Bounded because this is a browse surface, not an export; the
 *  header reports the untruncated total from `counts.owned` alongside it so a capped page can
 *  never read as "this is your whole library". */
const PAGE_LIMIT = 240;
/** `ChartCard` takes an explicit body height in px; the poster wall scrolls inside it. */
const PANEL_BODY_HEIGHT = 720;

type KindFilter = 'all' | 'movie' | 'show';

/** The guide's availability badge vocabulary (pattern library: "A title's state across the
 *  acquire → own → upgrade lifecycle"). Driven by the API's `availability` field. An
 *  unrecognized value renders VERBATIM rather than being coerced to a known state — mislabeling
 *  a title's availability is worse than showing an unfamiliar word. */
function availabilityLabel(availability: string): { text: string; tone: string } {
  switch (availability) {
    case 'on_disk':
      return { text: 'On disk', tone: 'var(--ok, #4ade80)' };
    case 'monitored':
      return { text: 'Wanted', tone: 'var(--info, #60a5fa)' };
    default:
      return { text: availability, tone: 'var(--text-200)' };
  }
}

function PosterTile({ item }: { item: MuseLibraryItem }) {
  const badge = availabilityLabel(item.availability);
  return (
    <div
      style={{
        display: 'flex',
        flexDirection: 'column',
        gap: 'var(--space-1)',
        minWidth: 0,
      }}
    >
      <div
        style={{
          position: 'relative',
          aspectRatio: '2 / 3',
          borderRadius: 'var(--radius-sm, 4px)',
          background: 'var(--space-600)',
          border: '1px solid var(--border)',
          overflow: 'hidden',
        }}
      >
        <img
          src={museArtUrl('media_metadata', String(item.media_metadata_id))}
          alt=""
          aria-hidden
          loading="lazy"
          style={{ width: '100%', height: '100%', objectFit: 'cover', display: 'block' }}
          // A title with no cached art degrades to the tile's own background rather than a
          // broken-image glyph. The tile keeps its shape either way.
          onError={e => {
            (e.currentTarget as HTMLImageElement).style.visibility = 'hidden';
          }}
        />
        <span
          style={{
            position: 'absolute',
            left: 4,
            bottom: 4,
            padding: '1px 6px',
            fontSize: 'var(--fs-2xs, 10px)',
            fontFamily: 'var(--font-mono)',
            color: badge.tone,
            background: 'rgba(0,0,0,0.72)',
            borderRadius: 'var(--radius-xs, 3px)',
          }}
        >
          {badge.text}
        </span>
      </div>
      <div
        title={item.title}
        style={{
          fontSize: 'var(--fs-xs)',
          color: 'var(--text-100)',
          lineHeight: 'var(--lh-tight)',
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
        }}
      >
        {item.title}
      </div>
      <div style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-300)', fontFamily: 'var(--font-mono)' }}>
        {/* `year: null` renders as the kind alone — never the string "null". */}
        {item.year !== null ? `${item.year} · ${item.kind}` : item.kind}
      </div>
    </div>
  );
}

export function LibraryPanel() {
  const { data, loading, degraded } = useMuseLibrary(PAGE_LIMIT);
  const [query, setQuery] = useState('');
  const [kind, setKind] = useState<KindFilter>('all');

  const owned = data?.owned ?? [];

  const visible = useMemo(() => {
    const q = query.trim().toLowerCase();
    return owned.filter(i => {
      if (kind !== 'all' && i.kind !== kind) return false;
      if (q && !i.title.toLowerCase().includes(q)) return false;
      return true;
    });
  }, [owned, query, kind]);

  // A successful call that returned no titles is an EMPTY library, which is a different thing
  // from a degraded endpoint — `ChartCard` renders them differently on purpose.
  const empty = !loading && !degraded && owned.length === 0;

  const total = data?.counts.owned ?? 0;
  // Say plainly when the page is capped. The alternative — showing 240 of 1892 with a bare
  // "240" — reads as "that's everything", which is the quiet-wrong-number failure.
  const countLabel = total > owned.length
    ? `${owned.length} of ${total} titles`
    : `${owned.length} titles`;

  return (
    <ChartCard
      title="Library"
      subtitle={
        data
          ? `${countLabel} · ${data.counts.on_disk} on disk${data.counts.wanted ? ` · ${data.counts.wanted} wanted` : ''}`
          : 'Poster wall'
      }
      // A tall fixed body: `ChartCard` needs an explicit px height, and the wall scrolls
      // INSIDE it (see the scroll container below) rather than growing the page.
      height={PANEL_BODY_HEIGHT}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No titles in the library"
      emptyHint="Run a library scan in Muse to populate the poster wall"
    >
      <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)', height: '100%', minHeight: 0 }}>
        {/* Filter row. Search is CLIENT-SIDE over the fetched page — it deliberately does not
            claim to search the whole library; server-side search is a follow-up. */}
        <div style={{ display: 'flex', gap: 'var(--space-2)', alignItems: 'center', flexWrap: 'wrap' }}>
          <input
            value={query}
            onChange={e => setQuery(e.target.value)}
            placeholder={`Search these ${owned.length} titles…`}
            aria-label="Search loaded titles"
            style={{
              flex: '1 1 220px',
              minWidth: 0,
              padding: '4px 8px',
              fontSize: 'var(--fs-xs)',
              fontFamily: 'var(--font-mono)',
              color: 'var(--text-100)',
              background: 'var(--space-700, var(--space-600))',
              border: '1px solid var(--border)',
              borderRadius: 'var(--radius-xs, 3px)',
            }}
          />
          {(['all', 'movie', 'show'] as KindFilter[]).map(k => (
            <button
              key={k}
              onClick={() => setKind(k)}
              aria-pressed={kind === k}
              style={{
                padding: '3px 10px',
                fontSize: 'var(--fs-2xs, 10px)',
                fontFamily: 'var(--font-mono)',
                textTransform: 'uppercase',
                letterSpacing: '0.04em',
                cursor: 'pointer',
                color: kind === k ? 'var(--text-000, #fff)' : 'var(--text-300)',
                background: kind === k ? 'var(--accent-dim, rgba(139,92,246,0.18))' : 'transparent',
                border: `1px solid ${kind === k ? 'var(--accent, #8b5cf6)' : 'var(--border)'}`,
                borderRadius: 'var(--radius-xs, 3px)',
              }}
            >
              {k === 'all' ? 'All' : k === 'movie' ? 'Movies' : 'Series'}
            </button>
          ))}
        </div>

        {/* THE scroll container. `minHeight: 0` is what lets it actually shrink inside the flex
            parent instead of pushing the page body into being the scroller. */}
        <div style={{ flex: 1, minHeight: 0, overflowY: 'auto', overflowX: 'hidden' }}>
          {visible.length === 0 && owned.length > 0 ? (
            <div style={{ padding: 'var(--space-3)', fontSize: 'var(--fs-xs)', color: 'var(--text-300)' }}>
              No titles match this filter.
            </div>
          ) : (
            <div
              style={{
                display: 'grid',
                gridTemplateColumns: 'repeat(auto-fill, minmax(112px, 1fr))',
                gap: 'var(--space-3)',
                paddingRight: 'var(--space-1)',
              }}
            >
              {visible.map(item => (
                <PosterTile key={item.media_item_id} item={item} />
              ))}
            </div>
          )}
        </div>
      </div>
    </ChartCard>
  );
}
