// MGUI-16 (S130): muse.request — search the metadata providers, see WHICH providers this
// deployment actually consulted, and file a request for something Muse does not hold.
//
// Registered as "Search & request" so it cannot be confused with `muse.requests`, which is
// the queue of requests that ALREADY EXIST. This page is the front of that pipeline.
//
// ── THE ENDPOINT ─────────────────────────────────────────────────────────────────────────
//
// `GET /api/muse/api/search?q=&kind=movie|series|all`. Its contract is final but it is NOT
// DEPLOYED YET (in review when this was built), so — unlike every other Muse panel in this
// directory — nothing below is transcribed from a live capture. See `useMuse.ts`'s MGUI-16
// section for the shapes. Until it ships, this page degrades exactly like any other unwired
// Muse route ("not yet wired"), which is the correct thing for it to say.
//
// ── WHY THE PROVIDER CATALOG IS BUILT FROM THE RESPONSE, NOT FROM A LIST ─────────────────
//
// The ask was "a catalog of various online metadata APIs so we can effectively search and
// request new content". The catalog here is the `providers` array of the LAST SEARCH: name,
// mode, configured, which kinds it can search, its status, and its per-kind counts. It is a
// description of the running deployment.
//
// A hardcoded roster (TMDb · TVDB · OMDb · Trakt · …) would look richer and be worthless: it
// would list providers this server has never heard of, and would keep listing one after it
// was removed. So before a search has run, the catalog says it has nothing to report rather
// than showing a shape it cannot vouch for — the provider list is only ever an OBSERVATION,
// and no observation exists until a query is made.
//
// ── WHY THE RESULT STATES ARE SPLIT SO FINELY ────────────────────────────────────────────
//
// On this deployment keyless TMDb is movies-only and keyless TVDB is series-only. Losing one
// provider therefore loses an ENTIRE MEDIA KIND, and a five-title list stops being a short
// answer and becomes half an answer — with nothing in `results` itself to say so. Six states
// with six different sentences, decided by the pure `searchOutcome` below:
//
//   idle             nothing was searched               (never "no results")
//   loading          a search is in flight
//   degraded         the fetch failed                   (never "no results")
//   unrecognized     2xx body we could not read         (never "no results")
//   incomplete-empty zero results AND coverage was lost (never "no matches")
//   no-matches       zero results, every provider healthy, every kind covered
//   results          at least one hit (caveats still shown above them)
//
// ── OWNERSHIP IS TRI-STATE, FOR THE SAME REASON ──────────────────────────────────────────
//
// `in_library` AND `in_catalog` are each `true | false | null`, and the `null` is not padding:
// `media_metadata.imdb_id` carries no uniqueness constraint (a plain index only), so several
// catalog rows can share one identifier while a provider hit is ONE title. When the hit cannot
// be pinned to a single row, "some row with this id is held" is not the same statement as "you
// hold this title", and the endpoint declines to make either. Three answers, three renderings,
// decided by `ownershipState`/`catalogState` — a `!in_library` truthiness test would read that
// refusal as "you do not have this" and offer a Request button for something the operator may
// already own.
//
// `resolution` then says WHY, and it is the field that separates the two negatives that used
// to be indistinguishable:
//
//   absent                 we looked and found nothing        -> a real negative
//   no_indexed_identifier  we never looked (no indexed id)    -> not a finding at all
//
// Both used to arrive as `in_library: false`. The tile now states which one it got — a checked
// negative says so explicitly, and an unchecked one is rendered as unknown, not as a "no". See
// `ownershipReason` and `isCheckedNegative`.
import { useCallback, useMemo, useState } from 'react';
import { ChartCard } from '../../viz/ChartCard';
import { Badge } from '../../components/Badge';
import { Button } from '../../components/Button';
import { RoleGate } from '../../components/RoleGate';
import {
  museArtUrlAt,
  museSearchResponse,
  useMuseAcquisitionGate,
  useMuseCreateRequest,
  useMuseSearch,
  type MuseSearchKind,
  type MuseSearchProvider,
  type MuseSearchResponse,
  type MuseSearchResult,
} from '../../hooks/useMuse';
import { gateResult } from './RequestLifecyclePanel';

const RESULTS_HEIGHT = 620;
const CATALOG_HEIGHT = 300;

// ── The honest-state decision, extracted and pure ────────────────────────────────────────

export type SearchState =
  | 'idle'
  | 'loading'
  | 'degraded'
  | 'unrecognized'
  | 'incomplete-empty'
  | 'no-matches'
  | 'results';

/** Something the result list does not say about itself. Each code gets its OWN sentence in
 *  the UI — they are different facts with different fixes, so sharing copy between them would
 *  be the same class of error as sharing copy between "empty" and "failed". */
export type SearchCaveat =
  | { code: 'uncovered-kinds'; kinds: string[]; complete: boolean }
  | { code: 'provider-error'; provider: string; messages: string[] }
  | { code: 'provider-partial'; provider: string; messages: string[] }
  | { code: 'truncated'; provider: string; kind: string; shown: number; providerReturned: number; limit: number };

/** Everything the response reports about ITSELF that the result list cannot show.
 *
 *  Messages are the providers' own `error` strings, collected verbatim — never summarized
 *  into a cause. A `partial`/`error` provider that reported no message yields an empty
 *  `messages` array, and the UI says the message was absent rather than inventing one. */
export function searchCaveats(resp: MuseSearchResponse): SearchCaveat[] {
  const out: SearchCaveat[] = [];
  // `complete: false` and a non-empty `uncovered_kinds` are one caveat, not two: they are the
  // same fact reported at two granularities. Either alone is enough to raise it — a server
  // that says "not complete" without naming a kind is still saying the answer is partial.
  if (!resp.complete || resp.uncovered_kinds.length > 0) {
    out.push({ code: 'uncovered-kinds', kinds: resp.uncovered_kinds, complete: resp.complete });
  }
  for (const p of resp.providers) {
    const messages = p.kinds.map(k => k.error).filter((m): m is string => typeof m === 'string' && m !== '');
    if (p.status === 'error') out.push({ code: 'provider-error', provider: p.name, messages });
    else if (p.status === 'partial') out.push({ code: 'provider-partial', provider: p.name, messages });
    for (const k of p.kinds) {
      // `truncated` is the SERVER's verdict. Not re-derived from `provider_returned > limit`:
      // only the server knows whether the upstream had more to give than it handed over.
      if (k.truncated) {
        out.push({
          code: 'truncated',
          provider: p.name,
          kind: k.kind,
          shown: k.result_count,
          providerReturned: k.provider_returned,
          limit: k.limit,
        });
      }
    }
  }
  return out;
}

/** True when a caveat means the result set is missing titles it should have contained. A
 *  TRUNCATION is deliberately not in this set: truncation happens because there were MORE
 *  matches, so it can never turn an empty list into an incomplete one. */
function isCoverageLoss(c: SearchCaveat): boolean {
  return c.code === 'uncovered-kinds' || c.code === 'provider-error' || c.code === 'provider-partial';
}

/**
 * What this page is entitled to SAY about a search, given the state of the fetch.
 *
 * Pure and exported for exactly the reason `gridState` is (ProgrammingGrid.tsx): the bug this
 * guards against is a CLAIM rendered about a response that does not exist, and there is no
 * DOM test harness in this package, so a component test cannot pin it.
 *
 * Order is the whole argument:
 *   idle outranks everything — with no submitted query there is no response to describe, and
 *     "no results found" for a search nobody ran is a fabricated observation;
 *   loading outranks the rest — nothing has been observed yet;
 *   degraded outranks unrecognized — a failed fetch has no body to fail to parse;
 *   unrecognized outranks both empties — an unreadable body is not an observed emptiness;
 *   incomplete-empty outranks no-matches — "no matches" asserts the providers looked and found
 *     nothing, which is false when a whole kind was never covered.
 */
export function searchOutcome(input: {
  /** Whether a query has actually been submitted. Not "is the input non-empty". */
  submitted: boolean;
  loading: boolean;
  degraded: { detail: string } | false;
  /** `null` = the body could not be read (or there is none). Never `[]`-shaped. */
  parsed: MuseSearchResponse | null;
}): { state: SearchState; detail: string | null; caveats: SearchCaveat[] } {
  if (!input.submitted) return { state: 'idle', detail: null, caveats: [] };
  if (input.loading) return { state: 'loading', detail: null, caveats: [] };
  if (input.degraded !== false) return { state: 'degraded', detail: input.degraded.detail, caveats: [] };
  if (input.parsed === null) return { state: 'unrecognized', detail: null, caveats: [] };

  const caveats = searchCaveats(input.parsed);
  if (input.parsed.results.length > 0) return { state: 'results', detail: null, caveats };
  return {
    state: caveats.some(isCoverageLoss) ? 'incomplete-empty' : 'no-matches',
    detail: null,
    caveats,
  };
}

/** Three states, never two. See `ownershipState`. */
export type Ownership = 'held' | 'not-held' | 'unknown';

/**
 * What this page may say about whether the operator already owns a hit.
 *
 * `in_library` is TRI-STATE: `null` means the hit's identifiers matched more than one catalog
 * row (`media_metadata.imdb_id` has no uniqueness constraint, only a plain index), so the
 * endpoint could not tell whether THIS title is held and declines to guess.
 *
 * The reason this is a function and not a `!r.in_library` in the tile: a truthiness test reads
 * `null` as "not held" and offers a Request button for something the operator may already own.
 * That is the exact bug the tri-state was introduced to prevent, and it is invisible in review
 * because the wrong branch renders perfectly.
 *
 * Written as explicit `=== true` / `=== false` comparisons so that `null`, `undefined`, and
 * anything non-boolean that survives the parser all land on `unknown` — the state that claims
 * the least — rather than on either assertion.
 */
export function ownershipState(r: { in_library: boolean | null }): Ownership {
  if (r.in_library === true) return 'held';
  if (r.in_library === false) return 'not-held';
  return 'unknown';
}

/** Three states again — `in_catalog` is tri-state for the same reason `in_library` is. */
export type CatalogState = 'in-catalog' | 'not-in-catalog' | 'unknown';

/**
 * Whether Muse KNOWS this title, as a three-way answer.
 *
 * The reason this is not `result.in_catalog && <Badge/>`: `null` is falsy, so a truthiness
 * test renders exactly what "not in the catalog" renders. That happens to be the safe
 * direction TODAY only because this page draws no negative catalog badge — the moment
 * someone adds one, a truthiness test starts asserting "Muse does not know this title" about
 * a hit whose catalog membership was never determined. The distinction is made here, once,
 * rather than depending on a rendering choice elsewhere staying the way it is.
 */
export function catalogState(r: { in_catalog: boolean | null }): CatalogState {
  if (r.in_catalog === true) return 'in-catalog';
  if (r.in_catalog === false) return 'not-in-catalog';
  return 'unknown';
}

/**
 * Why ownership could not be determined, in the response's own terms.
 *
 * Each resolution gets its OWN sentence, because each one sends an operator somewhere
 * different: `no_indexed_identifier` means Muse never looked (nothing to fix in the catalog),
 * `ambiguous_rows` means the catalog has several candidates (a dedupe problem),
 * `contradicted` means one candidate disagrees with the provider (a bad id somewhere). Giving
 * them shared copy would erase precisely the information this field was added to carry.
 *
 * An unrecognized resolution is reported AS THE WORD THE SERVER SENT, never mapped onto the
 * nearest known case — a resolution this page has not been taught is an unknown, and guessing
 * which of the five it resembles would be inventing a cause.
 */
export function ownershipReason(r: { resolution: string }): string {
  switch (r.resolution) {
    case 'no_indexed_identifier':
      return 'Muse could not check — this result carries no identifier it indexes (only TMDb, TVDB and IMDb ids are looked up). This is not a finding that you do not own it.';
    case 'ambiguous_rows':
      return 'Several catalog entries share this title’s identifier, so Muse could not tell which one this result is.';
    case 'contradicted':
      return 'Muse found one catalog entry, but an identifier stored on it disagrees with this result — so it did not treat them as the same title.';
    default:
      // Includes `settled`/`absent` arriving alongside a null flag, which is itself a state
      // this page cannot interpret. The word is shown so the operator can act on it even
      // though this build has no wording for it.
      return `Muse reported this result’s ownership as unresolved (“${r.resolution}”), a state this page has no specific explanation for.`;
  }
}

/** True when a definite `not-held` came from an actual lookup that found nothing — the one
 *  case where "you do not have this" is a MEASUREMENT rather than a default. Worth saying out
 *  loud precisely because `in_library: false` used to also mean "never looked". */
export function isCheckedNegative(r: { in_library: boolean | null; resolution: string }): boolean {
  return r.in_library === false && r.resolution === 'absent';
}

/**
 * The quality profile id, or `null` when the field cannot be used as one.
 *
 * Muse rejects `POST /requests` with 400 unless a `quality_profile_id` is supplied
 * (`has_matching_capability` needs Prowlarr configured AND a profile id), and there is NO
 * endpoint that lists the available profiles. Given the two honest options —
 *
 *   (a) a permanently disabled Request button explaining that no profile list is exposed, or
 *   (b) a numeric input for an id the operator already knows
 *
 * — this page takes (b), and the button stays disabled until this function returns a number.
 * (a) is honest but leaves the page unable to do the one thing it exists for, for an operator
 * who can read the profile id out of Radarr/Sonarr in ten seconds; (b) is equally honest as
 * long as it never PRETENDS to know the profiles, which is why the field is a bare number with
 * a note saying where the value has to come from, and not a dropdown of invented options.
 * Either way the button must never silently 400 — hence the disable, not a runtime surprise.
 *
 * A profile id is a positive integer row id. `0`, negatives, decimals and `1e3`-style input
 * are rejected here rather than being sent for Muse to reject.
 */
export function parseQualityProfileId(raw: string): number | null {
  const trimmed = raw.trim();
  if (!/^\d+$/.test(trimmed)) return null;
  const n = Number(trimmed);
  return Number.isSafeInteger(n) && n > 0 ? n : null;
}

// ── Small presentational pieces ──────────────────────────────────────────────────────────

const MONO: React.CSSProperties = { fontFamily: 'var(--font-mono)' };

function Note({ children }: { children: React.ReactNode }) {
  return (
    <div style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))', lineHeight: 1.5 }}>
      {children}
    </div>
  );
}

/** A prominent, non-decorative banner. Coverage loss is the one thing on this page that can
 *  make a correct-looking list a lie, so it is drawn as a bordered block above the results,
 *  never as a footnote under them. */
function CaveatBanner({ caveats }: { caveats: SearchCaveat[] }) {
  if (caveats.length === 0) return null;
  const severe = caveats.some(isCoverageLoss);
  const tone = severe ? 'var(--danger, #ff5a5a)' : 'var(--warn, #fbbf24)';
  return (
    <div
      // `alert` rather than `status`: a result list that is silently missing a whole media
      // kind is exactly the case a screen-reader user must not have to go looking for.
      role="alert"
      style={{
        display: 'flex',
        flexDirection: 'column',
        gap: 4,
        padding: 'var(--space-2)',
        border: `1px solid ${tone}`,
        borderRadius: 'var(--radius-xs, 3px)',
        background: 'rgba(0,0,0,0.18)',
        fontSize: 'var(--fs-xs)',
        color: 'var(--text-200)',
      }}
    >
      {caveats.map((c, i) => (
        <div key={i}>{caveatText(c)}</div>
      ))}
    </div>
  );
}

/** One sentence per caveat code, and deliberately no shared phrasing between them: a provider
 *  that FAILED, a kind that was never covered, and a list that was CUT SHORT are three
 *  different facts, and an operator fixes each of them somewhere else. */
function caveatText(c: SearchCaveat): string {
  switch (c.code) {
    case 'uncovered-kinds':
      return c.kinds.length > 0
        ? `Incomplete coverage — nothing below is from ${c.kinds.join(' or ')}. An entire media kind is missing from these results, so a short list here is not a complete one.`
        : 'Incomplete coverage — the server reported this search as incomplete without naming which kinds were missed. These results are a subset of what was asked for.';
    case 'provider-error':
      return c.messages.length > 0
        ? `Provider ${c.provider} FAILED: ${c.messages.join('; ')}. Its titles are missing from these results — this is a failure, not an absence of matches.`
        : `Provider ${c.provider} FAILED and reported no message. Its titles are missing from these results — this is a failure, not an absence of matches.`;
    case 'provider-partial':
      return c.messages.length > 0
        ? `Provider ${c.provider} answered PARTIALLY: ${c.messages.join('; ')}. Some of its titles may be missing.`
        : `Provider ${c.provider} answered PARTIALLY and reported no message. Some of its titles may be missing.`;
    case 'truncated':
      return `Truncated — ${c.provider} returned ${c.providerReturned} ${c.kind} hits and only ${c.shown} are shown (limit ${c.limit}). Narrow the search to see the rest.`;
  }
}

/** Provider status → a tone. An unrecognized status keeps the neutral tone and is rendered
 *  VERBATIM rather than coerced into one of the four known words. */
function statusTone(status: string): 'green' | 'amber' | 'rose' | 'neutral' {
  switch (status) {
    case 'ok':
      return 'green';
    case 'partial':
      return 'amber';
    case 'error':
      return 'rose';
    default:
      return 'neutral';
  }
}

function ProviderCard({ p }: { p: MuseSearchProvider }) {
  return (
    <div
      style={{
        display: 'flex',
        flexDirection: 'column',
        gap: 4,
        padding: 'var(--space-2)',
        border: '1px solid var(--border)',
        borderRadius: 'var(--radius-sm, 4px)',
        minWidth: 0,
      }}
    >
      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', justifyContent: 'space-between' }}>
        <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-100)' }}>{p.name}</span>
        <Badge tone={statusTone(p.status)} mono>
          {p.status}
        </Badge>
      </div>
      <div style={{ ...MONO, fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-300)' }}>
        {p.mode} · {p.configured ? 'configured' : 'not configured'}
      </div>
      <div style={{ ...MONO, fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))' }}>
        {/* CAN search vs WAS asked. A provider that can search series but was only asked for
            movies is healthy and contributed nothing — without both lists that reads as a
            provider that found nothing. */}
        can search: {p.searchable_kinds.length > 0 ? p.searchable_kinds.join(', ') : 'none reported'}
      </div>
      <div style={{ ...MONO, fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))' }}>
        searched: {p.searched_kinds.length > 0 ? p.searched_kinds.join(', ') : 'not consulted for this query'}
      </div>
      {p.kinds.map(k => (
        <div key={k.kind} style={{ ...MONO, fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-300)' }}>
          {k.kind}: {k.status} · {k.result_count} result{k.result_count === 1 ? '' : 's'}
          {k.truncated ? ` · truncated at ${k.limit} of ${k.provider_returned}` : ''}
          {/* The provider's own words, never paraphrased into a diagnosis. */}
          {k.error ? ` · ${k.error}` : ''}
        </div>
      ))}
    </div>
  );
}

// ── Result tile ──────────────────────────────────────────────────────────────────────────

/** Per-result outcome of a request POST. Kept per-result rather than page-wide so filing one
 *  title never labels another. */
type RequestState =
  | { phase: 'idle' }
  | { phase: 'submitting' }
  | { phase: 'filed' }
  | { phase: 'failed'; detail: string };

function ResultTile({
  result,
  requestState,
  qualityProfileId,
  onRequest,
}: {
  result: MuseSearchResult;
  requestState: RequestState;
  qualityProfileId: number | null;
  onRequest: () => void;
}) {
  const ids = result.provider_ids;
  const hasIds = ids !== null && ids !== undefined && Object.keys(ids).length > 0;

  // Art precedence, and it is not arbitrary:
  //   1. `media_metadata_id` -> Muse's OWN art, same-origin, on the rendition ladder (MUSE
  //      #100) — identical to the poster wall, so an owned title looks the same in both.
  //   2. `poster_url` -> the PROVIDER's absolute URL. A hit Muse does not hold has no
  //      media_metadata row and therefore no Muse art at all.
  //   3. neither -> the tile's own background. Never a broken-image glyph.
  const museArt = result.media_metadata_id !== null && result.media_metadata_id !== undefined
    ? museArtUrlAt('media_metadata', String(result.media_metadata_id), 160)
    : null;
  const art = museArt ?? result.poster_url;

  // NEVER `!result.in_library` — that reads a tri-state null as "not held". See ownershipState.
  const ownership = ownershipState(result);
  const catalog = catalogState(result);
  const requestable = ownership !== 'held' && hasIds && qualityProfileId !== null;

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 4, minWidth: 0 }}>
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
        {art !== null && (
          <img
            src={art}
            alt=""
            aria-hidden
            loading="lazy"
            // The provider poster is a THIRD-PARTY origin. No referrer, so browsing this
            // internal GUI does not hand TMDb the URL of the page you are on.
            referrerPolicy="no-referrer"
            style={{ width: '100%', height: '100%', objectFit: 'cover', display: 'block' }}
            onError={e => {
              (e.currentTarget as HTMLImageElement).style.visibility = 'hidden';
            }}
          />
        )}
        <span
          style={{
            position: 'absolute',
            left: 4,
            bottom: 4,
            padding: '1px 6px',
            ...MONO,
            fontSize: 'var(--fs-2xs, 10px)',
            color: 'var(--text-200)',
            background: 'rgba(0,0,0,0.72)',
            borderRadius: 'var(--radius-xs, 3px)',
          }}
        >
          {result.provider}
        </span>
      </div>

      <div
        title={result.title}
        style={{
          fontSize: 'var(--fs-xs)',
          color: 'var(--text-100)',
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
        }}
      >
        {result.title}
      </div>
      <div style={{ ...MONO, fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-300)' }}>
        {/* Only fields the hit actually carries. A null year/rating contributes nothing
            rather than a "—" that looks like a measured value. */}
        {[result.year !== null && result.year !== undefined ? String(result.year) : null, result.kind,
          result.rating !== null && result.rating !== undefined ? `★ ${result.rating}` : null]
          .filter(Boolean)
          .join(' · ')}
      </div>

      {/* Ownership, in THREE badges because there are three answers. `in_library` and
          `in_catalog` are not the same claim and never share one: in_library means Muse holds
          a file; in_catalog alone means Muse knows the title but does not hold it — which is
          precisely the requestable case. And `unknown` is neither of those: it is the endpoint
          declining to answer, which must not be dressed as a "no". */}
      <div style={{ display: 'flex', gap: 4, flexWrap: 'wrap' }}>
        {ownership === 'held' && <Badge tone="green">In library</Badge>}
        {ownership === 'unknown' && <Badge tone="amber">Ownership unknown</Badge>}
        {/* `catalogState`, never `result.in_catalog &&` — a null there must not render the
            same as a false. Only a definite `true` earns the claim. */}
        {ownership !== 'held' && catalog === 'in-catalog' && <Badge tone="blue">In catalog</Badge>}
      </div>

      {ownership === 'unknown' && (
        <Note>
          {/* The response's own reason, keyed on `resolution` — the field that exists to say
              whether Muse LOOKED, not merely what it found. */}
          {ownershipReason(result)} You can still request it — it may be a duplicate of
          something you own.
        </Note>
      )}

      {/* A definite negative that came from a real lookup. Said out loud because the same
          `in_library: false` used to be returned for hits that were never checked at all —
          without this line the operator cannot tell the two apart from the tile. */}
      {isCheckedNegative(result) && <Note>Checked against Muse’s indexed ids — no match.</Note>}

      {ownership === 'held' ? (
        // No Request control at all for a title already held — a disabled button here would
        // imply the request is merely blocked, when in fact there is nothing to request.
        <Note>Already held — nothing to request.</Note>
      ) : (
        <>
          <RoleGate>
            <Button
              variant="secondary"
              size="sm"
              disabled={!requestable || requestState.phase === 'submitting' || requestState.phase === 'filed'}
              onClick={onRequest}
              aria-describedby="muse-request-profile-note"
            >
              {requestState.phase === 'submitting'
                ? 'Filing…'
                : requestState.phase === 'filed'
                  ? 'Filed'
                  : 'Request'}
            </Button>
          </RoleGate>
          {/* Why a DISABLED button is disabled, per result. A control that is off for an
              unstated reason is indistinguishable from a broken one. */}
          {!hasIds && <Note>No provider ids on this hit, so Muse could not identify what to request.</Note>}
          {hasIds && qualityProfileId === null && <Note>Enter a quality profile id above to enable this.</Note>}
          {requestState.phase === 'filed' && <Note>Filed. It appears under Requests as `Requested`.</Note>}
          {requestState.phase === 'failed' && (
            <div style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--danger, #ff5a5a)', lineHeight: 1.5 }}>
              Request failed: {requestState.detail}
            </div>
          )}
        </>
      )}
    </div>
  );
}

// ── Panel ────────────────────────────────────────────────────────────────────────────────

const KINDS: { key: MuseSearchKind; label: string }[] = [
  { key: 'all', label: 'All' },
  { key: 'movie', label: 'Movies' },
  { key: 'series', label: 'Series' },
];

/** Stable per-result key. Index is included because two providers can legitimately return
 *  the same title/kind and there is no id guaranteed to be present on every hit. */
function resultKey(r: MuseSearchResult, idx: number): string {
  return `${idx}:${r.provider}:${r.kind}:${r.title}`;
}

export function RequestPanel() {
  // The DRAFT input and the SUBMITTED query are separate state on purpose: an empty `q` is a
  // 400, and searching per keystroke would fan every keystroke out to every upstream provider.
  const [draft, setDraft] = useState('');
  const [submitted, setSubmitted] = useState<string | null>(null);
  const [kind, setKind] = useState<MuseSearchKind>('all');
  const [profileRaw, setProfileRaw] = useState('');
  const [requestStates, setRequestStates] = useState<Record<string, RequestState>>({});

  const { data, loading, degraded } = useMuseSearch(submitted, kind);
  const { submit } = useMuseCreateRequest();

  const parsed = useMemo(() => museSearchResponse(data), [data]);
  const outcome = searchOutcome({
    submitted: submitted !== null && submitted.trim() !== '',
    loading,
    degraded,
    parsed,
  });
  const qualityProfileId = parseQualityProfileId(profileRaw);

  const runSearch = useCallback(() => {
    const q = draft.trim();
    if (q === '') return;
    // Clearing per-result request state on a new search: a "Filed" badge left over from the
    // previous query would be attached to a different title at the same grid position.
    setRequestStates({});
    setSubmitted(q);
  }, [draft]);

  const onRequest = useCallback(
    async (key: string, r: MuseSearchResult) => {
      const ids = r.provider_ids;
      if (ids === null || ids === undefined || qualityProfileId === null) return;
      setRequestStates(s => ({ ...s, [key]: { phase: 'submitting' } }));
      try {
        await submit({ provider_ids: ids, kind: r.kind, title: r.title, quality_profile_id: qualityProfileId });
        // The POST's RESPONSE BODY is deliberately not read: this branch was written against
        // an endpoint whose response shape has not been observed, and rendering a field from
        // it would be a guess. A resolved promise means a 2xx, which is the only thing being
        // claimed here.
        setRequestStates(s => ({ ...s, [key]: { phase: 'filed' } }));
      } catch (err) {
        setRequestStates(s => ({
          ...s,
          [key]: { phase: 'failed', detail: err instanceof Error ? err.message : 'unknown error' },
        }));
      }
    },
    [submit, qualityProfileId],
  );

  const results = parsed?.results ?? [];
  const providers = parsed?.providers ?? [];

  const chip = (active: boolean): React.CSSProperties => ({
    padding: '3px 10px',
    fontSize: 'var(--fs-2xs, 10px)',
    ...MONO,
    textTransform: 'uppercase',
    letterSpacing: '0.04em',
    cursor: 'pointer',
    color: active ? 'var(--text-000, #fff)' : 'var(--text-300)',
    background: active ? 'var(--accent-dim, rgba(139,92,246,0.18))' : 'transparent',
    border: `1px solid ${active ? 'var(--accent, #8b5cf6)' : 'var(--border)'}`,
    borderRadius: 'var(--radius-xs, 3px)',
  });

  return (
    <div style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      {/* Controls live ABOVE the cards, never inside a ChartCard (§4.3: filters are never
          card content). */}
      <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)' }}>
        <div style={{ display: 'flex', gap: 'var(--space-2)', alignItems: 'center', flexWrap: 'wrap' }}>
          <input
            value={draft}
            onChange={e => setDraft(e.target.value)}
            onKeyDown={e => {
              if (e.key === 'Enter') runSearch();
              if (e.key === 'Escape') {
                // Scoped to this field, never a window binding — the shell owns global keys
                // (see LibraryPanel's note on why `/` is not bound here).
                e.stopPropagation();
                setDraft('');
              }
            }}
            placeholder="Search the metadata providers…   (Enter searches, Esc clears)"
            aria-label="Search metadata providers"
            style={{
              flex: '1 1 280px',
              minWidth: 0,
              padding: '4px 8px',
              fontSize: 'var(--fs-xs)',
              ...MONO,
              color: 'var(--text-100)',
              background: 'var(--space-700, var(--space-600))',
              border: '1px solid var(--border)',
              borderRadius: 'var(--radius-xs, 3px)',
            }}
          />
          <Button variant="primary" size="sm" onClick={runSearch} disabled={draft.trim() === ''}>
            Search
          </Button>
          <span role="group" aria-label="Filter by kind" style={{ display: 'inline-flex', gap: 2, alignItems: 'center' }}>
            {KINDS.map(k => (
              <button key={k.key} onClick={() => setKind(k.key)} aria-pressed={kind === k.key} style={chip(kind === k.key)}>
                {k.label}
              </button>
            ))}
          </span>
        </div>

        <div style={{ display: 'flex', gap: 'var(--space-2)', alignItems: 'center', flexWrap: 'wrap' }}>
          <label htmlFor="muse-quality-profile" style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-300)', ...MONO }}>
            quality profile id
          </label>
          <input
            id="muse-quality-profile"
            value={profileRaw}
            onChange={e => setProfileRaw(e.target.value)}
            inputMode="numeric"
            placeholder="e.g. 1"
            aria-describedby="muse-request-profile-note"
            aria-invalid={profileRaw.trim() !== '' && qualityProfileId === null}
            style={{
              width: 90,
              padding: '4px 8px',
              fontSize: 'var(--fs-xs)',
              ...MONO,
              color: 'var(--text-100)',
              background: 'var(--space-700, var(--space-600))',
              border: `1px solid ${profileRaw.trim() !== '' && qualityProfileId === null ? 'var(--danger, #ff5a5a)' : 'var(--border)'}`,
              borderRadius: 'var(--radius-xs, 3px)',
            }}
          />
          <div id="muse-request-profile-note" style={{ flex: '1 1 320px', minWidth: 0 }}>
            <Note>
              Muse rejects a request without a <code style={MONO}>quality_profile_id</code>, and no endpoint
              lists the available profiles — so this is a bare id you take from your Radarr/Sonarr
              profile, not a picker. Request stays disabled until it is a positive integer, so it
              cannot 400 on you. {profileRaw.trim() !== '' && qualityProfileId === null && 'That is not a positive integer.'}
            </Note>
          </div>
        </div>

        <SafetyNote />
      </div>

      <ChartCard
        title="Results"
        subtitle={resultsSubtitle(outcome.state, submitted, results.length, providers.length)}
        height={RESULTS_HEIGHT}
        loading={outcome.state === 'loading'}
        // `degraded` is the fetch failing. Deliberately NOT reused for the other non-result
        // states — the card's degraded copy ("Module unavailable") would be a false diagnosis
        // for an unreadable body or an idle page.
        degraded={outcome.state === 'degraded' ? { detail: outcome.detail ?? 'unknown error' } : false}
      >
        <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-3)', height: '100%', minHeight: 0 }}>
          <CaveatBanner caveats={outcome.caveats} />

          {outcome.state === 'idle' && (
            <EmptyBlock
              headline="No search yet."
              body="Type a title above and press Enter. Nothing has been asked of any provider, so nothing is being claimed about what does or does not exist."
            />
          )}
          {outcome.state === 'unrecognized' && (
            <EmptyBlock
              headline="Search response could not be read."
              body="GET /api/search answered successfully, but the body was not a search response this page understands. No claim is made about how many titles matched — the response could not be parsed."
            />
          )}
          {outcome.state === 'incomplete-empty' && (
            <EmptyBlock
              headline="No results — but this search did not complete."
              body="The problems listed above mean at least one provider or media kind contributed nothing it should have. This is NOT a finding that there are no matches; it is a finding that part of the search did not happen."
            />
          )}
          {outcome.state === 'no-matches' && (
            <EmptyBlock
              headline={`No matches for “${parsed?.query ?? submitted ?? ''}”.`}
              body="Every consulted provider answered successfully and covered every requested kind, and none of them had this title. That is a real answer, not a failure."
            />
          )}

          {outcome.state === 'results' && (
            <div style={{ flex: 1, minHeight: 0, overflowY: 'auto' }}>
              <div
                style={{
                  display: 'grid',
                  gridTemplateColumns: 'repeat(auto-fill, minmax(132px, 1fr))',
                  gap: 'var(--space-3)',
                  paddingRight: 'var(--space-1)',
                }}
              >
                {results.map((r, i) => {
                  const key = resultKey(r, i);
                  return (
                    <ResultTile
                      key={key}
                      result={r}
                      requestState={requestStates[key] ?? { phase: 'idle' }}
                      qualityProfileId={qualityProfileId}
                      onRequest={() => void onRequest(key, r)}
                    />
                  );
                })}
              </div>
            </div>
          )}
        </div>
      </ChartCard>

      {/* The catalog sits BELOW the results because it describes the search that just ran —
          it is a report on this deployment's providers, not a navigation surface. */}
      <ChartCard
        title="Providers consulted"
        subtitle={
          providers.length > 0
            ? `${providers.length} reported by the last search`
            : 'reported per search by /api/search'
        }
        height={CATALOG_HEIGHT}
        loading={outcome.state === 'loading'}
        degraded={outcome.state === 'degraded' ? { detail: outcome.detail ?? 'unknown error' } : false}
      >
        <div style={{ height: '100%', overflowY: 'auto' }}>
          {providers.length === 0 ? (
            <EmptyBlock
              headline="No provider list yet."
              body="The provider catalog is part of the search response, so it describes the providers this deployment actually consulted for a specific query. Until a search runs there is nothing observed to list, and a hardcoded roster of metadata APIs would describe no server in particular."
            />
          ) : (
            <div
              style={{
                display: 'grid',
                gridTemplateColumns: 'repeat(auto-fill, minmax(220px, 1fr))',
                gap: 'var(--space-3)',
              }}
            >
              {providers.map(p => (
                <ProviderCard key={p.name} p={p} />
              ))}
            </div>
          )}
        </div>
      </ChartCard>
    </div>
  );
}

function EmptyBlock({ headline, body }: { headline: string; body: string }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)', padding: 'var(--space-3)', fontSize: 'var(--fs-xs)', color: 'var(--text-300)' }}>
      <div style={{ color: 'var(--text-100)' }}>{headline}</div>
      <div style={{ lineHeight: 1.55 }}>{body}</div>
    </div>
  );
}

/** The card subtitle, which must not contradict the body. Each state gets its own phrase for
 *  the same reason each empty state does. */
function resultsSubtitle(state: SearchState, submitted: string | null, count: number, providerCount: number): string {
  switch (state) {
    case 'idle':
      return 'nothing searched yet';
    case 'loading':
      return `searching ${submitted ? `“${submitted}”` : ''}`.trim();
    case 'degraded':
      return 'search endpoint unavailable';
    case 'unrecognized':
      return 'response not understood';
    case 'incomplete-empty':
      return 'search incomplete';
    case 'no-matches':
      return 'no matches';
    case 'results':
      return `${count} result${count === 1 ? '' : 's'} from ${providerCount} provider${providerCount === 1 ? '' : 's'}`;
  }
}

/**
 * Why filing a request from here is safe, split into what this surface can SEE and what it
 * can only cite.
 *
 * LIVE: gate 1 (`master_enabled && acquisition.enabled`) via the same `gateResult` the
 * lifecycle panel uses — imported rather than reimplemented so the two surfaces cannot drift
 * into disagreeing about what "safe" means.
 *
 * CITED, not read: no download client is configured on this deployment, so a filed request
 * cannot auto-grab and persists as `Requested` for operator review. That was verified when
 * this page was built (MGUI-16); NO endpoint reports download-client configuration, so this
 * page cannot re-check it at runtime and says so instead of presenting it as a live reading.
 */
function SafetyNote() {
  const { data, loading, degraded } = useMuseAcquisitionGate();
  // `null` is UNKNOWN, not off — a loading or degraded settings read must never render as a
  // definite negative on a safety gate.
  const gate1 = data ? data.master_enabled && data.acquisition.enabled : null;
  const safe = gateResult(gate1);

  return (
    <Note>
      Filing a request writes a <code style={MONO}>media_requests</code> row.{' '}
      {loading
        ? 'Reading the acquisition gate…'
        : degraded
          ? `The acquisition gate could not be read (${degraded.detail}), so this page cannot report its state.`
          : safe
            ? 'Gate 1 (acquisition) is OFF, so a request is persisted for review and never actioned — whatever gate 2 is.'
            : 'Gate 1 is not off and gate 2 (MUSE_ARR_REQUEST_AUTO_TIER_ENABLED) is not exposed to this surface, so the armed/safe verdict cannot be determined here.'}{' '}
      Separately, verified when this page was built: no download client is configured, so a
      request cannot auto-grab and stays <code style={MONO}>Requested</code> for operator review.
      No endpoint reports that, so it is a build-time observation, not a live reading.
    </Note>
  );
}
