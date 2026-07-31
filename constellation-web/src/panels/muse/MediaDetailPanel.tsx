// MGUI-03 (S129): muse.library.detail — the inspection bench. Guide screen 04.
//
// `GET /api/library/{id}` is on Muse's PUBLIC router and returns real data today:
// media_item, metadata (with provider images + ids), and the on-disk files with their
// media_info. Reached from a poster tile or a table row.
//
// THE CORRECTNESS RULE HERE IS OMISSION. The guide's mockup shows a match verdict
// ("✓ CONSISTENT · 0.94"), an enrichment cache, and a "More like this" vector-recall
// strip. On this deployment `match_verdict` is **null** and `enrichment` is **empty**
// for every title sampled. That is all it establishes — it does NOT establish that
// verify_match or the enrichment pass never ran (an earlier version of this comment
// claimed exactly that, and reviewers caught it twice). So those sections are OMITTED,
// not rendered blank and not defaulted:
//
//   - A defaulted "CONSISTENT" would assert that a file has been PROVEN to be what it
//     claims, on the strength of a field that is empty. That is a claim about the
//     operator's data integrity manufactured from an absence, and inventing it is the
//     worst thing this panel could do.
//   - An empty enrichment box would read as "we looked and found nothing", which is a
//     stronger claim than "nothing is recorded".
//
// Each omitted section leaves a one-line note stating the ABSENCE that was actually
// observed — "no verdict recorded", not a diagnosis of which pass did or did not run —
// so the gap is legible without inventing a cause for it.
import { useMemo } from 'react';
import { useParams } from 'react-router-dom';
import { ChartCard } from '../../viz/ChartCard';
import { useMuseMediaDetail, museArtUrl, museArtUrlAt, type MuseMediaFile } from '../../hooks/useMuse';

const PANEL_BODY_HEIGHT = 720;

/** Pull a display value out of the loosely-typed metadata blob without pretending the
 *  field is guaranteed. Returns null for absent/blank so callers can omit. */
function metaStr(meta: Record<string, unknown> | null, key: string): string | null {
  const v = meta?.[key];
  if (v === null || v === undefined) return null;
  const s = String(v).trim();
  return s === '' ? null : s;
}

function metaNum(meta: Record<string, unknown> | null, key: string): number | null {
  const v = meta?.[key];
  return typeof v === 'number' ? v : null;
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div style={{ display: 'flex', gap: 'var(--space-2)', fontSize: 'var(--fs-xs)' }}>
      <span style={{ color: 'var(--text-400, var(--text-300))', minWidth: 110, fontFamily: 'var(--font-mono)' }}>
        {label}
      </span>
      <span style={{ color: 'var(--text-100)', wordBreak: 'break-word' }}>{value}</span>
    </div>
  );
}

/** A section that exists in the design but whose data is absent. States the observed
 *  absence, never a diagnosis of why — an unexplained gap reads as a bug, but a
 *  wrongly-diagnosed one sends the operator to fix the wrong thing.
 *
 *  Named `AbsenceNote`, not `NotRunNote`: the old name encoded the very inference
 *  (that some pass did not run) that the copy was corrected to stop making. A helper
 *  whose name contradicts its contract invites the next author to reintroduce it. */
function AbsenceNote({ what }: { what: string }) {
  return (
    <div style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))', fontStyle: 'italic' }}>
      {what}
    </div>
  );
}

function FileRow({ f }: { f: MuseMediaFile }) {
  const info = (f.media_info ?? {}) as Record<string, unknown>;
  const bits = ['container', 'video_codec', 'audio_codec', 'resolution', 'width', 'height']
    .map(k => (info[k] === undefined || info[k] === null ? null : `${k}=${String(info[k])}`))
    .filter(Boolean) as string[];
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 2, padding: '4px 0', borderBottom: '1px solid var(--border-subtle, rgba(255,255,255,0.05))' }}>
      <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-100)', wordBreak: 'break-all' }}>{f.relative_path}</div>
      <div style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-300)', fontFamily: 'var(--font-mono)' }}>
        {/* Only the media_info keys that are actually present — no invented "unknown"s. */}
        {bits.length ? bits.join(' · ') : 'no media_info recorded'}
        {f.release_group ? ` · ${f.release_group}` : ''}
        {f.edition ? ` · ${f.edition}` : ''}
      </div>
    </div>
  );
}

export function MediaDetailPanel() {
  const { id } = useParams<{ id: string }>();
  const { data, loading, degraded } = useMuseMediaDetail(id ?? null);

  const meta = data?.metadata ?? null;
  const title = metaStr(meta, 'title') ?? '—';
  const year = metaNum(meta, 'year');
  const kind = metaStr(meta, 'kind');
  const overview = metaStr(meta, 'overview');
  const runtime = metaNum(meta, 'runtime_minutes');
  const studio = metaStr(meta, 'studio');
  const network = metaStr(meta, 'network');

  const providerIds = useMemo(
    () =>
      (['tmdb_id', 'tvdb_id', 'imdb_id'] as const)
        .map(k => [k.replace('_id', '').toUpperCase(), metaStr(meta, k)] as const)
        .filter(([, v]) => v !== null) as [string, string][],
    [meta],
  );

  // `found: false` is a real not-found, distinct from a degraded endpoint.
  const notFound = !loading && !degraded && data !== null && data.found === false;
  // Defaulted because a found:false (or partial) response omits these arrays entirely
  // — dereferencing them threw instead of letting the not-found state render.
  const files = data?.files ?? [];
  const enrichment = data?.enrichment ?? [];
  const artId = metaNum(meta, 'id');

  return (
    <ChartCard
      title={loading ? 'Loading…' : title}
      subtitle={[year, kind, runtime ? `${runtime} min` : null, studio ?? network]
        .filter(Boolean)
        .join(' · ')}
      height={PANEL_BODY_HEIGHT}
      loading={loading}
      degraded={degraded}
      empty={notFound}
      emptyMessage="Title not found"
      emptyHint="This media item id is not in the library"
    >
      <div style={{ display: 'flex', flexDirection: 'column', height: '100%', minHeight: 0, overflowY: 'auto' }}>
        {/* The guide's backdrop band. `backdrop_url` was in the payload all along and
            went unused in the first cut (reviewer finding). Rendered behind a soft
            fade so the content below stays readable; `aria-hidden` because it carries
            no information the text does not already give. */}
        {artId !== null && (
          <div
            aria-hidden
            style={{
              position: 'relative',
              height: 140,
              marginBottom: 'var(--space-3)',
              borderRadius: 'var(--radius-sm, 4px)',
              overflow: 'hidden',
              background: 'var(--space-600)',
              flex: '0 0 auto',
            }}
          >
            <img
              src={`${museArtUrl('media_metadata', String(artId))}?variant=fanart`}
              alt=""
              style={{ width: '100%', height: '100%', objectFit: 'cover', display: 'block', opacity: 0.55 }}
              onError={e => {
                (e.currentTarget as HTMLImageElement).style.visibility = 'hidden';
              }}
            />
            <div
              style={{
                position: 'absolute',
                inset: 0,
                background: 'linear-gradient(to bottom, rgba(0,0,0,0.1), var(--space-800, #0b0b10))',
              }}
            />
          </div>
        )}

      <div style={{ display: 'flex', gap: 'var(--space-4)', flex: 1, minHeight: 0 }}>
        <div style={{ flex: '0 0 200px' }}>
          {artId !== null && (
            <img
              src={museArtUrlAt('media_metadata', String(artId), 320)}
              alt=""
              aria-hidden
              style={{
                width: '100%',
                aspectRatio: '2 / 3',
                objectFit: 'cover',
                borderRadius: 'var(--radius-sm, 4px)',
                border: '1px solid var(--border)',
                background: 'var(--space-600)',
              }}
              onError={e => {
                (e.currentTarget as HTMLImageElement).style.visibility = 'hidden';
              }}
            />
          )}
        </div>

        <div style={{ flex: 1, minWidth: 0, display: 'flex', flexDirection: 'column', gap: 'var(--space-3)' }}>
          {overview && (
            <div>
              <div style={{ fontSize: 'var(--fs-2xs, 10px)', textTransform: 'uppercase', letterSpacing: '0.04em', color: 'var(--text-400, var(--text-300))', marginBottom: 4 }}>
                Overview
              </div>
              <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-100)', lineHeight: 1.5 }}>{overview}</div>
            </div>
          )}

          {providerIds.length > 0 && (
            <div>
              <div style={{ fontSize: 'var(--fs-2xs, 10px)', textTransform: 'uppercase', letterSpacing: '0.04em', color: 'var(--text-400, var(--text-300))', marginBottom: 4 }}>
                Provider ids
              </div>
              {providerIds.map(([k, v]) => (
                <Row key={k} label={k} value={v} />
              ))}
            </div>
          )}

          <div>
            <div style={{ fontSize: 'var(--fs-2xs, 10px)', textTransform: 'uppercase', letterSpacing: '0.04em', color: 'var(--text-400, var(--text-300))', marginBottom: 4 }}>
              {/* `files` is ABSENT (not empty) on a found:false response, so this must
                  not dereference `.length` — codex caught it crashing the panel instead
                  of rendering the intended not-found state. */}
              On disk · {files.length} file{files.length === 1 ? '' : 's'}
            </div>
            {files.length > 0 ? (
              files.map(f => <FileRow key={f.id} f={f} />)
            ) : (
              <AbsenceNote what="No files recorded for this title." />
            )}
          </div>

          {/* The two guide sections whose fields come back null/empty here. Omitted
              rather than defaulted — see the module doc. */}
          <div>
            <div style={{ fontSize: 'var(--fs-2xs, 10px)', textTransform: 'uppercase', letterSpacing: '0.04em', color: 'var(--text-400, var(--text-300))', marginBottom: 4 }}>
              Match verdict
            </div>
            {data?.match_verdict ? (
              <Row label="verdict" value={JSON.stringify(data.match_verdict)} />
            ) : (
              <AbsenceNote what="No verdict recorded for this file. Nothing is implied about whether it matches — absence of a verdict is not a verdict." />
            )}
          </div>

          <div>
            <div style={{ fontSize: 'var(--fs-2xs, 10px)', textTransform: 'uppercase', letterSpacing: '0.04em', color: 'var(--text-400, var(--text-300))', marginBottom: 4 }}>
              Enrichment
            </div>
            {enrichment.length > 0 ? (
              <Row label="entries" value={String(enrichment.length)} />
            ) : (
              <AbsenceNote what="No cached enrichment recorded for this title." />
            )}
          </div>
        </div>
      </div>
      </div>
    </ChartCard>
  );
}
