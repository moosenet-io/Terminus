// MGUI-16 (S130): muse.request — search the metadata providers, see which providers this
// deployment reported (and what each of them did with the query), and request something Muse
// does not hold.
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
// The heading is "Providers REPORTED" (`CATALOG_TITLE`), not "consulted": the array
// deliberately includes providers with `status: "not_consulted"`, because a kind-filtered
// search excludes whole providers and that status exists to say so.
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
//   idle                nothing was searched                  (never "no results")
//   loading             a search is in flight
//   degraded            the fetch failed                      (never "no results")
//   unrecognized        2xx body we could not read            (never "no results")
//   incomplete-empty    zero results AND coverage was lost    (never "no matches")
//   indeterminate-empty zero results AND something in the response could not be interpreted
//   no-matches          zero results, nothing notable in the response at all
//   results             at least one hit (caveats still shown above them)
//
// `no-matches` is the ONLY definitive claim this page makes about an absence, so it is the one
// state that must be EARNED: any caveat at all withholds it. The provider catalog card derives
// its own state (`providerCatalogState`) rather than reading `providers.length`, for the same
// reason — an empty array after a failed search is not the same fact as an empty array before
// one was run.
//
// ── FACTS THIS PAGE WAS TOLD ARE NOT FACTS IT OBSERVED ───────────────────────────────────
//
// One rendered claim on this page comes from the operator rather than from a response: that no
// download client is configured here. It is almost certainly true, and it is still not a
// measurement — no endpoint reports it, nothing re-checks it, and it would go on rendering
// after someone configured one. It is therefore rendered as an attributed, dated, visually
// separated build-time note (`BUILD_TIME_NOTE`), never folded into a sentence beside a live
// reading. A page whose whole contract is "only say what you can support" cannot make a quiet
// exception for a true fact, because a reader has no way to tell which claims got the
// exception. Everything else on this page comes from a response.
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

/** The catalog heading. A CONSTANT rather than an inline string so the rule can be pinned by a
 *  test: the array deliberately includes providers with `status: "not_consulted"` (a
 *  kind-filtered search excludes whole providers, and that status exists to say so), so a
 *  heading claiming they were consulted is contradicted by the rows beneath it. */
export const CATALOG_TITLE = 'Providers reported';

/** What a successful POST is allowed to say. Extracted for the same reason `BUILD_TIME_NOTE`
 *  is: the response body is deliberately never parsed, so a resolved promise establishes a
 *  2xx and nothing else — not that a row persisted, not where it is visible, not what status
 *  it landed in. The previous copy named a status AND a location, neither of which was read. */
export const REQUEST_ACCEPTED_NOTE =
  'Muse accepted the request. This page does not read the response, so it reports nothing about where the request went or what state it is in.';

// ── The honest-state decision, extracted and pure ────────────────────────────────────────

export type SearchState =
  | 'idle'
  | 'loading'
  | 'degraded'
  | 'unrecognized'
  | 'incomplete-empty'
  | 'indeterminate-empty'
  | 'no-matches'
  | 'results';

/** Something the result list does not say about itself. Each code gets its OWN sentence in
 *  the UI — they are different facts with different fixes, so sharing copy between them would
 *  be the same class of error as sharing copy between "empty" and "failed". */
export type SearchCaveat =
  | { code: 'uncovered-kinds'; kinds: string[]; complete: boolean }
  | { code: 'provider-error'; provider: string; messages: string[] }
  | { code: 'provider-partial'; provider: string; messages: string[] }
  | { code: 'kind-error'; provider: string; kind: string; message: string | null }
  | { code: 'kind-partial'; provider: string; kind: string; message: string | null }
  | { code: 'unknown-status'; scope: 'provider' | 'kind'; provider: string; kind: string | null; status: string }
  | { code: 'truncated'; provider: string; kind: string; shown: number; providerReturned: number; limit: number }
  /** The response contradicts itself about whether anything was found. `basis` says which
   *  field disagrees with the empty result list — they are separate observations and a
   *  response can carry both. */
  | { code: 'contradictory-empty'; basis: 'truncated' | 'positive-count' };

/** The status vocabulary this page has wording for. Deliberately NOT enforced by the parser —
 *  a sixth value must not break the page — but a status outside this set cannot be
 *  INTERPRETED either, and an uninterpretable status is not evidence of success. */
const KNOWN_STATUSES = new Set(['ok', 'partial', 'error', 'not_consulted']);

/** Everything the response reports about ITSELF that the result list cannot show.
 *
 *  Messages are the providers' own `error` strings, collected verbatim — never summarized
 *  into a cause. A `partial`/`error` provider that reported no message yields an empty
 *  `messages` array, and the UI says the message was absent rather than inventing one.
 *
 *  Read at BOTH levels. The provider-level `status` is a ROLLUP, and this page must not
 *  depend on the rollup being right: a `kinds[].status` of `error`/`partial` under a
 *  provider-level `ok` is either an internally inconsistent payload or a server that rolls up
 *  differently than today's does. Either way the page was rendering "every consulted provider
 *  answered successfully" over data that said otherwise — a definitive claim contradicted by
 *  the very response it came from. Trusting one field to summarize another correctly is how a
 *  false-green survives, so both are read.
 */
export function searchCaveats(resp: MuseSearchResponse): SearchCaveat[] {
  const out: SearchCaveat[] = [];
  // `complete: false` and a non-empty `uncovered_kinds` are one caveat, not two: they are the
  // same fact reported at two granularities. Either alone is enough to raise it — a server
  // that says "not complete" without naming a kind is still saying the answer is partial.
  if (!resp.complete || resp.uncovered_kinds.length > 0) {
    out.push({ code: 'uncovered-kinds', kinds: resp.uncovered_kinds, complete: resp.complete });
  }
  for (const p of resp.providers) {
    const kindCaveats: SearchCaveat[] = [];
    for (const k of p.kinds) {
      const message = typeof k.error === 'string' && k.error !== '' ? k.error : null;
      if (k.status === 'error') kindCaveats.push({ code: 'kind-error', provider: p.name, kind: k.kind, message });
      else if (k.status === 'partial') kindCaveats.push({ code: 'kind-partial', provider: p.name, kind: k.kind, message });
      else if (!KNOWN_STATUSES.has(k.status)) {
        kindCaveats.push({ code: 'unknown-status', scope: 'kind', provider: p.name, kind: k.kind, status: k.status });
      }
    }

    out.push(...kindCaveats);

    // The provider-level failure is SUPPRESSED only when the kind level REPORTS THE SAME
    // FAILURE — i.e. when a kind-level caveat is itself an error/partial. The kind-level
    // entry then names the kind and carries that kind's own message, so it is strictly more
    // specific, and printing both would duplicate every real error (today's server rolls a
    // provider up to `partial` whenever a kind errors).
    //
    // Suppressing on "any kind caveat exists" was wrong and swallowed real errors: a provider
    // with `status: "error"` whose only kind caveat was an UNRECOGNIZED STATUS had its error
    // silently dropped, because an uninterpretable status is not a report of that failure —
    // it is the absence of one. A more specific sentence may replace a less specific one;
    // nothing may replace a failure it does not itself state.
    const kindReportsFailure = kindCaveats.some(c => c.code === 'kind-error' || c.code === 'kind-partial');
    const messages = p.kinds.map(k => k.error).filter((m): m is string => typeof m === 'string' && m !== '');
    if (p.status === 'error' && !kindReportsFailure) {
      out.push({ code: 'provider-error', provider: p.name, messages });
    } else if (p.status === 'partial' && !kindReportsFailure) {
      out.push({ code: 'provider-partial', provider: p.name, messages });
    }
    // A provider-level status that is uninterpretable is always said, whatever the kind level
    // reported — it is a different fact, not a rollup of one.
    if (!KNOWN_STATUSES.has(p.status)) {
      out.push({ code: 'unknown-status', scope: 'provider', provider: p.name, kind: null, status: p.status });
    }

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

  // ── A RESPONSE THAT DISAGREES WITH ITSELF ABOUT FINDING ANYTHING ─────────────────────────
  //
  // Both checks below compare the empty `results` list against a field that says something WAS
  // found. Whatever produced such a payload, the page does not get to resolve the
  // contradiction in favour of the confident reading — see `searchOutcome`.
  if (resp.results.length === 0) {
    // Truncation means the provider returned MORE than the limit, which cannot be true of a
    // search that returned nothing.
    if (out.some(c => c.code === 'truncated')) {
      out.push({ code: 'contradictory-empty', basis: 'truncated' });
    }
    // A reported count above zero says a provider found something; an empty `results` says
    // nothing came back. Checked at BOTH levels — the per-kind counts and the provider's own
    // rollup are separate fields and either one disagreeing is enough. Previously neither was
    // read, so `result_count: 12` alongside `results: []` rendered as the definitive "none of
    // the providers had this title".
    const positiveCount = resp.providers.some(p => p.result_count > 0 || p.kinds.some(k => k.result_count > 0));
    if (positiveCount) {
      out.push({ code: 'contradictory-empty', basis: 'positive-count' });
    }
  }
  return out;
}

/** True when a caveat means the result set is MISSING titles it should have contained.
 *
 *  This is narrower than "blocks a no-matches claim" (see `searchOutcome`) and they are not
 *  the same question. A TRUNCATION is deliberately not in this set: truncation happens because
 *  there were MORE matches, so it can never turn an empty list into an incomplete one. Nor is
 *  an UNKNOWN STATUS: a status the page cannot interpret is not evidence that anything was
 *  lost — it is an absence of evidence either way, which earns the weaker sentence, not this
 *  one. Both still withhold the definitive negative; they just do not get to assert coverage
 *  loss they have not observed. */
function isCoverageLoss(c: SearchCaveat): boolean {
  return (
    c.code === 'uncovered-kinds' ||
    c.code === 'provider-error' ||
    c.code === 'provider-partial' ||
    c.code === 'kind-error' ||
    c.code === 'kind-partial'
  );
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
  // Zero results. `no-matches` is the ONLY definitive claim this page ever makes about an
  // absence, so it is the one that has to be earned: it requires a response with nothing
  // notable about it at all. Coverage loss earns the strong sentence ("part of the search did
  // not happen"); ANY other caveat earns the weak one ("this cannot be confirmed as empty").
  //
  // `caveats.length > 0` rather than a second predicate listing the blocking codes, and that
  // is deliberate: a caveat added later by someone who never reads this comment then defaults
  // to WITHHOLDING the confident negative rather than to permitting it. The failure direction
  // of a forgotten update is the safe one.
  if (caveats.some(isCoverageLoss)) return { state: 'incomplete-empty', detail: null, caveats };
  if (caveats.length > 0) return { state: 'indeterminate-empty', detail: null, caveats };
  return { state: 'no-matches', detail: null, caveats };
}

/** What the PROVIDER CATALOG card may say, which is not the same question as what the results
 *  card may say. */
export type CatalogSectionState =
  | 'idle'
  | 'loading'
  | 'degraded'
  | 'unrecognized'
  | 'no-providers-reported'
  | 'providers';

/**
 * The catalog's own state.
 *
 * This exists because the card previously showed "until a search runs there is nothing to
 * list" for EVERY empty provider array — including after a failed search, after an unreadable
 * body, and after a perfectly good 2xx that carried no providers. Three of those four are not
 * "we have not searched yet", and the copy asserted that we had not.
 *
 * The last case is worth its own state rather than being folded into the idle one: the
 * endpoint reports every provider it knows about on every search, so a completed search
 * carrying an EMPTY provider list is anomalous. The card says that it is anomalous — and
 * nothing about why, because nothing in the response reports why.
 */
export function providerCatalogState(searchState: SearchState, providerCount: number): CatalogSectionState {
  switch (searchState) {
    case 'idle':
      return 'idle';
    case 'loading':
      return 'loading';
    case 'degraded':
      return 'degraded';
    case 'unrecognized':
      return 'unrecognized';
    default:
      // Every remaining state means a response was read successfully, so the provider array is
      // an OBSERVATION — empty or not.
      return providerCount === 0 ? 'no-providers-reported' : 'providers';
  }
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
    case 'kind-error':
      // Names the KIND, which the provider-level sentence cannot. On this deployment a lost
      // kind is a lost provider, so this is the specific version of the same danger.
      return c.message !== null
        ? `${c.provider} FAILED on ${c.kind}: ${c.message}. No ${c.kind} results from it are in this list — that is a failure, not an absence of matches.`
        : `${c.provider} FAILED on ${c.kind} and reported no message. No ${c.kind} results from it are in this list — that is a failure, not an absence of matches.`;
    case 'kind-partial':
      return c.message !== null
        ? `${c.provider} answered PARTIALLY for ${c.kind}: ${c.message}. Some of its ${c.kind} titles may be missing.`
        : `${c.provider} answered PARTIALLY for ${c.kind} and reported no message. Some of its ${c.kind} titles may be missing.`;
    case 'unknown-status':
      // Shown verbatim and interpreted as NOTHING. An unrecognized status is not a success,
      // and it is not a failure either — it is a word this build has no meaning for.
      return c.scope === 'kind'
        ? `${c.provider} reported an unrecognized status for ${c.kind} (“${c.status}”). This page cannot interpret it, so it draws no conclusion from that provider’s ${c.kind} outcome — including no conclusion that it searched successfully.`
        : `Provider ${c.provider} reported an unrecognized status (“${c.status}”). This page cannot interpret it, so it draws no conclusion about whether that provider searched successfully.`;
    case 'truncated':
      return `Truncated — ${c.provider} returned ${c.providerReturned} ${c.kind} hits and only ${c.shown} are shown (limit ${c.limit}). Narrow the search to see the rest.`;
    case 'contradictory-empty':
      return c.basis === 'truncated'
        ? 'This response reports a truncated result set but carries no results at all. Those cannot both be true, so the empty list is not treated as a finding either way.'
        : 'This response reports a result count above zero but carries no results at all. Those cannot both be true, so the empty list is not treated as a finding either way.';
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
/** `accepted`, not `filed`: the POST response body is deliberately never read (its shape has
 *  not been observed), so a resolved promise establishes a 2xx and NOTHING ELSE. It does not
 *  establish that a row persisted, that it is visible anywhere, or what state it landed in. */
type RequestState =
  | { phase: 'idle' }
  | { phase: 'submitting' }
  | { phase: 'accepted' }
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
              disabled={!requestable || requestState.phase === 'submitting' || requestState.phase === 'accepted'}
              onClick={onRequest}
              aria-describedby="muse-request-profile-note"
            >
              {requestState.phase === 'submitting'
                ? 'Sending…'
                : requestState.phase === 'accepted'
                  ? 'Accepted'
                  : 'Request'}
            </Button>
          </RoleGate>
          {/* Why a DISABLED button is disabled, per result. A control that is off for an
              unstated reason is indistinguishable from a broken one. */}
          {!hasIds && <Note>No provider ids on this hit, so Muse could not identify what to request.</Note>}
          {hasIds && qualityProfileId === null && <Note>Enter a quality profile id above to enable this.</Note>}
          {/* Says only what a 2xx supports. The earlier copy ("Filed. It appears under
              Requests as `Requested`.") named a persisted STATUS and a LOCATION, neither of
              which was read from anything — the response body is deliberately not parsed. */}
          {requestState.phase === 'accepted' && (
            <Note>{REQUEST_ACCEPTED_NOTE}</Note>
          )}
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
        // claimed here — hence `accepted`, not `filed`.
        setRequestStates(s => ({ ...s, [key]: { phase: 'accepted' } }));
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
  // The catalog card's state is DERIVED SEPARATELY, not inferred from `providers.length`: an
  // empty array means different things after an idle page, a failed fetch, an unreadable body
  // and a completed search, and only one of those is "we have not searched yet".
  const catalogSection = providerCatalogState(outcome.state, providers.length);

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
          {outcome.state === 'indeterminate-empty' && (
            <EmptyBlock
              headline="No results — and this page cannot confirm that means nothing matched."
              body="The notes above describe something in the response that could not be interpreted cleanly. Reading an empty list as “nothing matched” would be a stronger claim than this response supports, so it is not made."
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
          it is a report on this deployment's providers, not a navigation surface.

          "Providers REPORTED", not "consulted": the array deliberately includes providers with
          `status: "not_consulted"` (a kind-filtered search excludes whole providers, and that
          status exists precisely to say so). Titling the card "consulted" made a claim that
          the rows underneath it contradict. */}
      <ChartCard
        title={CATALOG_TITLE}
        subtitle={
          catalogSection === 'providers'
            ? `${providers.length} reported by the last search`
            : 'reported per search by /api/search'
        }
        height={CATALOG_HEIGHT}
        loading={outcome.state === 'loading'}
        degraded={outcome.state === 'degraded' ? { detail: outcome.detail ?? 'unknown error' } : false}
      >
        <div style={{ height: '100%', overflowY: 'auto' }}>
          {catalogSection === 'idle' ? (
            <EmptyBlock
              headline="No provider list yet."
              body="The provider catalog is part of the search response, so it describes the providers this deployment reported for a specific query — including any it did not consult for that query. Until a search runs there is nothing observed to list, and a hardcoded roster of metadata APIs would describe no server in particular."
            />
          ) : catalogSection === 'unrecognized' ? (
            <EmptyBlock
              headline="Provider list could not be read."
              body="The search returned a body this page could not parse, so the provider list it may or may not have contained was never read. This is not a statement that no providers are configured."
            />
          ) : catalogSection === 'no-providers-reported' ? (
            <EmptyBlock
              headline="The search reported no providers at all."
              body="This is a completed search, not a pending one: the response was read successfully and its provider list was empty. The endpoint reports every provider it knows about on every search, so an empty list here is anomalous. Nothing is claimed about the cause — the response does not report one."
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
    case 'indeterminate-empty':
      return 'no results · not confirmed empty';
    case 'no-matches':
      return 'no matches';
    case 'results':
      return `${count} result${count === 1 ? '' : 's'} from ${providerCount} provider${providerCount === 1 ? '' : 's'}`;
  }
}

/**
 * The acquisition-gate sentence, derived ONLY from the live `/api/settings` read.
 *
 * Pure and exported so the one rule that matters here is testable: this string may contain
 * nothing that was not read from that endpoint. Loading, unreadable, provably-safe and
 * indeterminate are four different sentences; `null` gate is UNKNOWN, never a definite
 * negative on a safety control.
 */
export function acquisitionGateReadout(input: {
  loading: boolean;
  /** The degrade detail, or `null` when the read succeeded. */
  degradedDetail: string | null;
  /** `master_enabled && acquisition.enabled`, or `null` when it could not be read. */
  gate1: boolean | null;
}): string {
  if (input.loading) return 'Reading the acquisition gate…';
  if (input.degradedDetail !== null) {
    return `The acquisition gate could not be read (${input.degradedDetail}), so this page cannot report its state.`;
  }
  // `gateResult` is imported rather than reimplemented so this surface and the lifecycle
  // panel cannot drift into disagreeing about what "safe" means.
  return gateResult(input.gate1) === 'safe'
    ? 'Gate 1 (acquisition) is OFF, so a request is persisted for review and never actioned — whatever gate 2 is.'
    : 'Gate 1 is not off and gate 2 (MUSE_ARR_REQUEST_AUTO_TIER_ENABLED) is not exposed to this surface, so the armed/safe verdict cannot be determined here.';
}

/**
 * A fact this page did NOT measure, rendered as such.
 *
 * The operator stated, when MGUI-16 was built, that no download client is configured on this
 * deployment. It is very probably true. It is still not an observation: no endpoint reports
 * download-client configuration, nothing here re-checks it, and it would keep rendering
 * unchanged after someone configured one.
 *
 * The previous copy folded it into the same sentence as the live gate reading, which made a
 * fact handed over in a prompt look like something the page had measured. Attribution and
 * date are part of the claim, not decoration around it — a page whose entire contract is
 * "only say what you can support" cannot make an exception for a fact that happens to be
 * true, because the reader has no way to tell which facts got the exception.
 */
export const BUILD_TIME_NOTE =
  'Build-time note, not measured by this page: when this page was built (MGUI-16), the operator reported that no download client is configured on this deployment, so an accepted request cannot be auto-grabbed. Nothing here re-checks that — no endpoint reports it — so it may be out of date.';

function SafetyNote() {
  const { data, loading, degraded } = useMuseAcquisitionGate();
  // `null` is UNKNOWN, not off — a loading or degraded settings read must never render as a
  // definite negative on a safety gate.
  const gate1 = data ? data.master_enabled && data.acquisition.enabled : null;

  return (
    <>
      <Note>
        Requesting sends a write to Muse.{' '}
        {acquisitionGateReadout({
          loading,
          degradedDetail: degraded === false ? null : degraded.detail,
          gate1,
        })}
      </Note>
      {/* Visually separated, not merely a clause later in the same sentence — the split is
          the point. Everything above is read from /api/settings; this is not. */}
      <div
        style={{
          fontSize: 'var(--fs-2xs, 10px)',
          color: 'var(--text-400, var(--text-300))',
          lineHeight: 1.5,
          borderLeft: '2px solid var(--border)',
          paddingLeft: 'var(--space-2)',
          fontStyle: 'italic',
        }}
      >
        {BUILD_TIME_NOTE}
      </div>
    </>
  );
}
