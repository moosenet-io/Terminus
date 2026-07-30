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
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ChartCard } from '../../viz/ChartCard';
import { useMuseLibrary, useMuseLibraryTable, museArtUrlAt, type MuseLibraryItem } from '../../hooks/useMuse';
import { LibraryTableView } from './LibraryTableView';

/** How many titles to request. Bounded because this is a browse surface, not an export; the
 *  header reports the untruncated total from `counts.owned` alongside it so a capped page can
 *  never read as "this is your whole library". */
const PAGE_LIMIT = 240;
/** `ChartCard` takes an explicit body height in px; the poster wall scrolls inside it. */
const PANEL_BODY_HEIGHT = 720;
/** The table view is denser than the grid, so it can afford more rows per fetch. */
const TABLE_LIMIT = 500;

/** The guide's filter chips are Movies · Series · Wanted · Unwatched. Three of the four are here.
 *  **`Unwatched` is deliberately ABSENT**, not forgotten: `/api/library` projects no watched state,
 *  so the chip would have nothing to filter on. The data exists (84 media_items have a finished
 *  play_session; watch_stats has 146 rows) but is not exposed by this endpoint — tracked as
 *  MUSE #101. Shipping a chip that silently filters nothing is worse than shipping three that work.
 *
 *  The guide's `sort: taste` control is absent for the same reason (no per-title fit score in this
 *  projection — same issue), and the tile's `★` rating is absent because `media_metadata.ratings`
 *  and `popularity` are 0-populated across all 1886 rows, so it would render blank for every title
 *  (MUSE #102). */
/** Kind and availability are INDEPENDENT axes, not one mutually-exclusive chip
 *  row — "movies I don't have yet" is a real question the old single-row filter
 *  could not express. */
type KindFilter = 'all' | 'movie' | 'show';
type AvailFilter = 'all' | 'on_disk' | 'wanted';
type View = 'grid' | 'table';

/** Sorts over data that ACTUALLY EXISTS in the payload. The guide also asks for a
 *  taste sort; there is no per-title fit score in this projection, so it is absent
 *  rather than faked (MUSE #101) — same rule that kept the star rating out. */
type SortKey = 'title_asc' | 'title_desc' | 'year_desc' | 'year_asc';

const SORTS: { key: SortKey; label: string }[] = [
  { key: 'title_asc', label: 'A→Z' },
  { key: 'title_desc', label: 'Z→A' },
  { key: 'year_desc', label: 'Newest' },
  { key: 'year_asc', label: 'Oldest' },
];

const ALPHABET = ['#', ...'ABCDEFGHIJKLMNOPQRSTUVWXYZ'.split('')];

/** The alphabet bucket a title sorts into. Leading articles are stripped so
 *  "The Martian" lands under M, which is where someone looking for it will press
 *  — matching how Plex/Jellyfin index a library. Anything not starting with a
 *  letter buckets under '#'. */
export function alphaKey(title: string): string {
  const stripped = title.trim().replace(/^(the|a|an)\s+/i, '');
  const first = stripped.charAt(0).toUpperCase();
  return first >= 'A' && first <= 'Z' ? first : '#';
}

/** Sort title: the same article-stripped, case-folded form the alphabet index
 *  uses, so the A→Z order and the letter rail agree. If they disagreed, pressing
 *  M would scroll to a position where M does not start. */
function sortTitle(title: string): string {
  return title.trim().replace(/^(the|a|an)\s+/i, '').toLowerCase();
}

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
          // MGUI-15: a 160px RENDITION, not the master (MUSE #100). The master is
          // ~1.9 MB for a ~112px tile, which is what made the first poster wall
          // fill in slowly.
          src={museArtUrlAt('media_metadata', String(item.media_metadata_id), 160)}
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
  const [avail, setAvail] = useState<AvailFilter>('all');
  const [sort, setSort] = useState<SortKey>('title_asc');
  const [view, setView] = useState<View>('grid');
  const table = useMuseLibraryTable(TABLE_LIMIT, view === 'table');

  const scrollerRef = useRef<HTMLDivElement | null>(null);
  const searchRef = useRef<HTMLInputElement | null>(null);
  // One ref per rendered letter bucket, so a jump scrolls to a real element
  // rather than an estimated offset (tile heights vary with title wrapping).
  const letterAnchors = useRef<Record<string, HTMLDivElement | null>>({});

  const owned = data?.owned ?? [];

  const matches = useCallback(
    (item: { kind: string; title: string; availability?: string; on_disk?: boolean }) => {
      if (kind !== 'all' && item.kind !== kind) return false;
      if (avail !== 'all') {
        // The grid rows carry `availability`; the table rows carry `on_disk`.
        // Normalise rather than duplicating the filter for each shape.
        const onDisk =
          item.availability !== undefined ? item.availability === 'on_disk' : item.on_disk === true;
        if (avail === 'on_disk' && !onDisk) return false;
        if (avail === 'wanted' && onDisk) return false;
      }
      const q = query.trim().toLowerCase();
      if (q && !item.title.toLowerCase().includes(q)) return false;
      return true;
    },
    [kind, avail, query],
  );

  const compare = useCallback(
    (a: { title: string; year: number | null }, b: { title: string; year: number | null }) => {
      switch (sort) {
        case 'title_desc':
          return sortTitle(b.title).localeCompare(sortTitle(a.title));
        case 'year_desc':
        case 'year_asc': {
          // A title with no year sorts LAST in both directions rather than
          // pretending to be year 0 (which would bury real 2026 titles under
          // unknowns, or float unknowns to the top).
          const ay = a.year;
          const by = b.year;
          if (ay === null && by === null) return sortTitle(a.title).localeCompare(sortTitle(b.title));
          if (ay === null) return 1;
          if (by === null) return -1;
          if (ay !== by) return sort === 'year_desc' ? by - ay : ay - by;
          return sortTitle(a.title).localeCompare(sortTitle(b.title));
        }
        case 'title_asc':
        default:
          return sortTitle(a.title).localeCompare(sortTitle(b.title));
      }
    },
    [sort],
  );

  const visible = useMemo(
    () => owned.filter(matches).slice().sort(compare),
    [owned, matches, compare],
  );

  const tableRows = useMemo(
    () => (table.data ?? []).filter(matches).slice().sort(compare),
    [table.data, matches, compare],
  );

  /** Which letters actually have a title in the CURRENT result set. A letter with
   *  nothing behind it is rendered inert rather than as a button that does
   *  nothing when pressed. */
  const presentLetters = useMemo(() => {
    const set = new Set<string>();
    for (const i of visible) set.add(alphaKey(i.title));
    return set;
  }, [visible]);

  /** The alphabet rail only makes sense while the list is in title order — under
   *  a year sort, "jump to M" has no position to jump to. */
  const alphaActive = sort === 'title_asc' || sort === 'title_desc';

  const jumpTo = useCallback((letter: string) => {
    const el = letterAnchors.current[letter];
    const scroller = scrollerRef.current;
    if (!el || !scroller) return;
    // Scroll by the measured DELTA between the two boxes.
    //
    // The previous form was `scrollTop = el.offsetTop - scroller.offsetTop`.
    // That happened to produce the same result here, but it is not sound:
    // `offsetTop` is measured from the nearest POSITIONED ancestor, which is not
    // guaranteed to be this scroller, so the two values need not share a
    // coordinate space and subtracting them is only accidentally correct.
    // `getBoundingClientRect` puts both in viewport space, where the difference
    // IS the distance to scroll — correct by construction rather than by luck.
    //
    // (I first reported this as a landing bug on the strength of a faulty probe
    // that looked for the first visible CAPTION; a caption sits below its poster,
    // so a previous row whose poster had scrolled off still matched. Measuring
    // the anchor tile directly shows the landing was, and is, exact — offset 0
    // for every letter tried.)
    //
    // Still the CONTAINER's scrollTop, never `scrollIntoView` — that walks up to
    // the nearest scrollable ancestor and can drag the whole shell.
    const delta = el.getBoundingClientRect().top - scroller.getBoundingClientRect().top;
    scroller.scrollTop += delta;
  }, []);

  const filtersActive = kind !== 'all' || avail !== 'all' || query.trim() !== '';
  const clearFilters = useCallback(() => {
    setKind('all');
    setAvail('all');
    setQuery('');
  }, []);

  // NO global key binding here, deliberately.
  //
  // The first version bound `/` to focus this panel's search — but the SHELL
  // already owns `/` for its global tool search (App.tsx), so the two handlers
  // competed and the panel lost: verification showed `/` never focused this
  // input at all. Stealing a shell-wide shortcut for one panel would be wrong
  // even if it had won, because it would break the same key everywhere else in
  // the app.
  //
  // Esc-to-clear is bound on the INPUT itself (below) rather than on `window`,
  // so it is scoped to the field the user is actually typing in and cannot
  // interfere with anything outside this panel.

  // A successful call that returned no titles is an EMPTY library, which is a different thing
  // from a degraded endpoint — `ChartCard` renders them differently on purpose.
  const empty = !loading && !degraded && owned.length === 0;

  const chip = (active: boolean): React.CSSProperties => ({
    padding: '3px 10px',
    fontSize: 'var(--fs-2xs, 10px)',
    fontFamily: 'var(--font-mono)',
    textTransform: 'uppercase',
    letterSpacing: '0.04em',
    cursor: 'pointer',
    color: active ? 'var(--text-000, #fff)' : 'var(--text-300)',
    background: active ? 'var(--accent-dim, rgba(139,92,246,0.18))' : 'transparent',
    border: `1px solid ${active ? 'var(--accent, #8b5cf6)' : 'var(--border)'}`,
    borderRadius: 'var(--radius-xs, 3px)',
  });

  const shown = view === 'table' ? tableRows.length : visible.length;
  const total = data?.counts.owned ?? 0;
  // Per-VIEW loaded count. The grid and the table are separate endpoints with
  // separate page sizes (240 vs 500), so a single `loaded` produced a
  // self-contradictory subtitle in table view — "500 of 240 loaded" (codex).
  const loaded = view === 'table' ? (table.data?.length ?? 0) : owned.length;
  // The search placeholder must agree with whichever view is on screen.
  const searchScopeCount = loaded;

  return (
    <ChartCard
      title="Library"
      subtitle={
        data
          ? `${shown}${filtersActive ? ' matching' : ''} of ${loaded} loaded · ${total} in library · ${data.counts.on_disk} on disk`
          : 'Poster wall'
      }
      height={PANEL_BODY_HEIGHT}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No titles in the library"
      emptyHint="Run a library scan in Muse to populate the poster wall"
    >
      <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)', height: '100%', minHeight: 0 }}>
        {/* Row 1 — search + view toggle. Search is CLIENT-SIDE over the loaded
            page and its placeholder says so rather than implying full-library
            search. */}
        <div style={{ display: 'flex', gap: 'var(--space-2)', alignItems: 'center' }}>
          <input
            ref={searchRef}
            value={query}
            onChange={e => setQuery(e.target.value)}
            onKeyDown={e => {
              // Scoped to this field — see the note above on why there is no
              // window-level binding.
              if (e.key === 'Escape') {
                e.stopPropagation();
                setQuery('');
              }
            }}
            placeholder={`Search these ${searchScopeCount} titles…   (Esc clears)`}
            aria-label="Search loaded titles"
            style={{
              flex: '1 1 240px',
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
          {filtersActive && (
            <button onClick={clearFilters} style={chip(false)} title="Clear search and filters">
              clear
            </button>
          )}
          <span style={{ display: 'inline-flex', gap: 2 }}>
            {(['grid', 'table'] as View[]).map(v => (
              <button key={v} onClick={() => setView(v)} aria-pressed={view === v} style={chip(view === v)}>
                {v}
              </button>
            ))}
          </span>
        </div>

        {/* Row 2 — the two INDEPENDENT filter axes plus sort. Kept as separate
            groups so "movies I don't own yet" is expressible; a single chip row
            could not say that. */}
        <div style={{ display: 'flex', gap: 'var(--space-3)', alignItems: 'center', flexWrap: 'wrap' }}>
          {/* Each axis is a labelled group. Without this a screen reader meets
              several ambiguous buttons — two of them effectively "everything" —
              with no way to tell which axis they belong to (codex). */}
          <span role="group" aria-label="Filter by kind" style={{ display: 'inline-flex', gap: 2, alignItems: 'center' }}>
            <span aria-hidden style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))', marginRight: 4 }}>kind</span>
            {(['all', 'movie', 'show'] as KindFilter[]).map(k => (
              <button
                key={k}
                onClick={() => setKind(k)}
                aria-pressed={kind === k}
                aria-label={k === 'all' ? 'All kinds' : k === 'movie' ? 'Movies' : 'Series'}
                style={chip(kind === k)}
              >
                {k === 'all' ? 'All' : k === 'movie' ? 'Movies' : 'Series'}
              </button>
            ))}
          </span>
          <span role="group" aria-label="Filter by availability" style={{ display: 'inline-flex', gap: 2, alignItems: 'center' }}>
            <span aria-hidden style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))', marginRight: 4 }}>have</span>
            {(['all', 'on_disk', 'wanted'] as AvailFilter[]).map(a => (
              <button
                key={a}
                onClick={() => setAvail(a)}
                aria-pressed={avail === a}
                aria-label={a === 'all' ? 'Any availability' : a === 'on_disk' ? 'On disk' : 'Wanted'}
                style={chip(avail === a)}
              >
                {a === 'all' ? 'Any' : a === 'on_disk' ? 'On disk' : 'Wanted'}
              </button>
            ))}
          </span>
          <span role="group" aria-label="Sort order" style={{ display: 'inline-flex', gap: 2, alignItems: 'center' }}>
            <span aria-hidden style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))', marginRight: 4 }}>sort</span>
            {SORTS.map(sd => (
              <button key={sd.key} onClick={() => setSort(sd.key)} aria-pressed={sort === sd.key} style={chip(sort === sd.key)}>
                {sd.label}
              </button>
            ))}
          </span>
        </div>

        {/* Body: the grid (with its alphabet rail) or the table. */}
        {view === 'table' ? (
          table.degraded ? (
            <div style={{ padding: 'var(--space-3)', fontSize: 'var(--fs-xs)', color: 'var(--text-300)' }}>
              Table view unavailable: {table.degraded.detail}
            </div>
          ) : (
            <LibraryTableView rows={tableRows} />
          )
        ) : (
          <div style={{ display: 'flex', gap: 'var(--space-2)', flex: 1, minHeight: 0 }}>
            <div
              ref={scrollerRef}
              role="region"
              aria-label="Library poster grid"
              tabIndex={0}
              style={{ flex: 1, minHeight: 0, overflowY: 'auto', overflowX: 'hidden' }}
            >
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
                  {visible.map((item, idx) => {
                    // Anchor the FIRST tile of each letter bucket so a jump
                    // targets a real element rather than an estimated offset —
                    // tile heights vary with title wrapping.
                    const key = alphaKey(item.title);
                    const isFirstOfLetter = idx === 0 || alphaKey(visible[idx - 1].title) !== key;
                    return (
                      <div
                        key={item.media_item_id}
                        ref={
                          isFirstOfLetter
                            ? el => {
                                letterAnchors.current[key] = el;
                              }
                            : undefined
                        }
                        style={{ minWidth: 0 }}
                      >
                        <PosterTile item={item} />
                      </div>
                    );
                  })}
                </div>
              )}
            </div>

            {/* The A–Z jump rail. Only meaningful in title order, so under a year
                sort it is hidden rather than shown as a row of inert letters. */}
            {alphaActive && (
              <div
                role="navigation"
                aria-label="Jump to letter"
                style={{
                  display: 'flex',
                  flexDirection: 'column',
                  gap: 0,
                  overflow: 'hidden',
                  flex: '0 0 auto',
                  paddingLeft: 2,
                }}
              >
                {ALPHABET.map(letter => {
                  const has = presentLetters.has(letter);
                  return (
                    <button
                      key={letter}
                      onClick={() => has && jumpTo(letter)}
                      disabled={!has}
                      aria-disabled={!has}
                      title={has ? `Jump to ${letter}` : `No titles under ${letter}`}
                      style={{
                        // A letter with nothing behind it is visibly inert, not a
                        // button that silently does nothing when pressed.
                        cursor: has ? 'pointer' : 'default',
                        color: has ? 'var(--text-200)' : 'var(--text-500, rgba(255,255,255,0.18))',
                        background: 'transparent',
                        border: 'none',
                        padding: '0 3px',
                        lineHeight: 1.15,
                        fontSize: 'var(--fs-2xs, 10px)',
                        fontFamily: 'var(--font-mono)',
                      }}
                    >
                      {letter}
                    </button>
                  );
                })}
              </div>
            )}
          </div>
        )}
      </div>
    </ChartCard>
  );
}
