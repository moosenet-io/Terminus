// MGUI-16: the rules that keep the Request page from claiming things the search response does
// not support. Every test below pins a DISTINCTION — loading vs idle vs failed vs unreadable vs
// incomplete vs genuinely empty — rather than arithmetic. Collapsing any two of those is the
// bug this page exists to avoid, and they collapse silently: they all render "nothing here".
import { describe, it, expect } from 'vitest';
import { museSearchResponse, type MuseSearchResponse } from '../../hooks/useMuse';
import {
  searchCaveats,
  searchOutcome,
  ownershipState,
  catalogState,
  providerCatalogState,
  ownershipReason,
  isCheckedNegative,
  parseQualityProfileId,
} from './RequestPanel';

const hit = (over: Partial<MuseSearchResponse['results'][number]> = {}) => ({
  provider: 'tmdb',
  kind: 'movie',
  title: 'The Martian',
  year: 2015,
  overview: 'x',
  first_aired: '2015-09-30',
  rating: 7.7,
  provider_ids: { tmdb: '286217' },
  poster_url: null,
  in_library: false as boolean | null,
  in_catalog: false as boolean | null,
  ambiguous_match: false as boolean | null,
  resolution: 'absent',
  media_metadata_id: null,
  ...over,
});

const providerKind = (over: Partial<MuseSearchResponse['providers'][number]['kinds'][number]> = {}) => ({
  kind: 'movie',
  status: 'ok',
  error: null,
  result_count: 1,
  truncated: false,
  provider_returned: 1,
  limit: 40,
  ...over,
});

const provider = (over: Partial<MuseSearchResponse['providers'][number]> = {}) => ({
  name: 'tmdb',
  mode: 'radarr_proxy',
  configured: true,
  searchable_kinds: ['movie'],
  searched_kinds: ['movie'],
  status: 'ok',
  kinds: [providerKind()],
  result_count: 1,
  ...over,
});

const response = (over: Partial<MuseSearchResponse> = {}): MuseSearchResponse => ({
  query: 'martian',
  requested_kinds: ['movie', 'series'],
  providers: [provider()],
  complete: true,
  uncovered_kinds: [],
  results: [hit()],
  ...over,
});

describe('museSearchResponse — a body it cannot read is null, never an empty search', () => {
  it('parses the contract shape', () => {
    const parsed = museSearchResponse(response());
    expect(parsed).not.toBeNull();
    expect(parsed?.results).toHaveLength(1);
    expect(parsed?.providers).toHaveLength(1);
  });

  it('keeps results:[] meaning exactly one thing — the server searched and found nothing', () => {
    // This is the load-bearing distinction on the whole page. If an unreadable body also
    // produced an empty list, "no matches for X" would be printed about a payload nobody
    // understood — the same failure `museChannelList` was corrected for (MGUI-10).
    expect(museSearchResponse(response({ results: [] }))?.results).toEqual([]);
    expect(museSearchResponse({ nope: true })).toBeNull();
    expect(museSearchResponse(null)).toBeNull();
    expect(museSearchResponse(undefined)).toBeNull();
  });

  it('does not throw on a scalar 2xx body', () => {
    // `'query' in data` THROWS on a primitive, and `useMuseSection` hands any 2xx body
    // through as data — so a bare `true`/`42`/`"x"` reaches this parser for real.
    expect(() => museSearchResponse(true)).not.toThrow();
    expect(museSearchResponse(true)).toBeNull();
    expect(museSearchResponse(42)).toBeNull();
    expect(museSearchResponse('x')).toBeNull();
    expect(museSearchResponse([])).toBeNull();
  });

  it('validates ELEMENTS, not just container shape', () => {
    // `[null]` reached the render path and threw on `r.title` / `p.name`; `[{}]` rendered a
    // tile with an undefined title and an undefined status badge. Neither array is empty and
    // neither carries a result/provider, so the body is unreadable.
    expect(museSearchResponse(response({ results: [null] as never }))).toBeNull();
    expect(museSearchResponse(response({ results: [{}] as never }))).toBeNull();
    expect(museSearchResponse(response({ providers: [null] as never }))).toBeNull();
    expect(museSearchResponse(response({ providers: [{}] as never }))).toBeNull();
    // One bad element invalidates the batch; a good one alongside it does not rescue it.
    expect(museSearchResponse(response({ results: [hit(), null as never] }))).toBeNull();
    // Nested elements count too — a provider whose per-kind entries are junk is unreadable.
    expect(museSearchResponse(response({ providers: [provider({ kinds: [null] as never })] }))).toBeNull();
  });

  it('rejects a non-boolean ownership flag rather than coercing it', () => {
    // in_library decides whether a Request button is offered at all. A truthy string would
    // offer a request for a title already on disk; an absent field would offer one for a
    // title whose ownership was never reported.
    expect(museSearchResponse(response({ results: [hit({ in_library: 'yes' as never })] }))).toBeNull();
    expect(museSearchResponse(response({ results: [hit({ in_library: undefined as never })] }))).toBeNull();
    expect(museSearchResponse(response({ results: [hit({ in_catalog: undefined as never })] }))).toBeNull();
    expect(museSearchResponse(response({ results: [hit({ in_catalog: 1 as never })] }))).toBeNull();
  });

  it('keeps a NULL in_catalog too — it is tri-state for the same reason in_library is', () => {
    const parsed = museSearchResponse(response({
      results: [hit({ in_library: null, in_catalog: null, resolution: 'ambiguous_rows' })],
    }));
    expect(parsed?.results[0].in_catalog).toBeNull();
  });

  it('requires resolution to be present and non-empty', () => {
    // It is the ONLY field distinguishing a checked negative from an unchecked one, so a hit
    // without it cannot be rendered honestly at all.
    const { resolution: _omitted, ...withoutResolution } = hit();
    expect(museSearchResponse(response({ results: [withoutResolution as never] }))).toBeNull();
    expect(museSearchResponse(response({ results: [hit({ resolution: '' })] }))).toBeNull();
    expect(museSearchResponse(response({ results: [hit({ resolution: '   ' })] }))).toBeNull();
    expect(museSearchResponse(response({ results: [hit({ resolution: null as never })] }))).toBeNull();
    expect(museSearchResponse(response({ results: [hit({ resolution: 3 as never })] }))).toBeNull();
  });

  it('accepts a resolution value it has never seen rather than rejecting the response', () => {
    // The server may grow a sixth case. Rejecting an otherwise perfect response over one
    // unknown word would take the whole page down for a cosmetic reason; the panel says it
    // has no wording for it instead (see ownershipReason).
    const parsed = museSearchResponse(response({
      results: [hit({ in_library: null, resolution: 'some_future_state' })],
    }));
    expect(parsed?.results[0].resolution).toBe('some_future_state');
  });

  it('keeps a NULL in_library as null — it is a legal value, not a defect', () => {
    // The tri-state: null means the hit matched several catalog rows, so ownership was not
    // answered. Rejecting it would make an ordinary response unreadable; coercing it to false
    // would assert something the endpoint explicitly refused to assert.
    const parsed = museSearchResponse(response({ results: [hit({ in_library: null, ambiguous_match: true })] }));
    expect(parsed?.results[0].in_library).toBeNull();
    expect(parsed?.results[0].ambiguous_match).toBe(true);
  });

  it('tolerates an absent ambiguous_match by normalizing it to null', () => {
    // Explanatory field only — it explains a state `in_library` already reports, so its
    // absence must not make the whole search unreadable.
    const { ambiguous_match: _omitted, ...withoutFlag } = hit({ in_library: null });
    const parsed = museSearchResponse(response({ results: [withoutFlag as never] }));
    expect(parsed?.results[0].ambiguous_match).toBeNull();
    expect(museSearchResponse(response({ results: [hit({ ambiguous_match: 'maybe' as never })] }))).toBeNull();
  });

  it('rejects provider_ids that could not be sent as a JSON object body', () => {
    expect(museSearchResponse(response({ results: [hit({ provider_ids: ['tmdb', '1'] as never })] }))).toBeNull();
    expect(museSearchResponse(response({ results: [hit({ provider_ids: 'tmdb:1' as never })] }))).toBeNull();
    // Genuinely absent ids are fine — the tile renders and explains why it cannot be requested.
    expect(museSearchResponse(response({ results: [hit({ provider_ids: null })] }))).not.toBeNull();
  });
});

describe('searchOutcome — six states, never sharing a meaning', () => {
  const base = { submitted: true, loading: false, degraded: false as const, parsed: museSearchResponse(response()) };

  it('is idle before a search is submitted, whatever else is true', () => {
    // "No results found" for a search nobody ran is a fabricated observation. Idle outranks
    // every other signal because there is no response to describe at all.
    expect(searchOutcome({ ...base, submitted: false }).state).toBe('idle');
    expect(searchOutcome({ ...base, submitted: false, parsed: null }).state).toBe('idle');
  });

  it('ranks loading above degraded, and degraded above unrecognized', () => {
    expect(searchOutcome({ ...base, loading: true, parsed: null }).state).toBe('loading');
    expect(searchOutcome({ ...base, degraded: { detail: 'HTTP 500 for /api/muse/api/search' }, parsed: null }).state).toBe('degraded');
    // A failed fetch has no body, so it must never be reported as an unreadable one.
    expect(searchOutcome({ ...base, degraded: { detail: 'boom' }, parsed: null }).detail).toBe('boom');
  });

  it('reports an unreadable body as unrecognized, not as empty', () => {
    expect(searchOutcome({ ...base, parsed: null }).state).toBe('unrecognized');
  });

  it('separates a genuinely empty search from an incomplete one', () => {
    const healthyEmpty = museSearchResponse(response({ results: [], providers: [provider({ kinds: [providerKind({ result_count: 0, provider_returned: 0 })], result_count: 0 })] }));
    expect(searchOutcome({ ...base, parsed: healthyEmpty }).state).toBe('no-matches');

    // An entire kind missing: on this deployment that is one provider dying, and the result
    // list has nothing in it to say so.
    const uncovered = museSearchResponse(response({ results: [], complete: false, uncovered_kinds: ['series'] }));
    expect(searchOutcome({ ...base, parsed: uncovered }).state).toBe('incomplete-empty');

    // A provider that FAILED is not an absence of matches either.
    const failed = museSearchResponse(response({ results: [], providers: [provider({ status: 'error' })] }));
    expect(searchOutcome({ ...base, parsed: failed }).state).toBe('incomplete-empty');

    const partial = museSearchResponse(response({ results: [], providers: [provider({ status: 'partial' })] }));
    expect(searchOutcome({ ...base, parsed: partial }).state).toBe('incomplete-empty');
  });

  it('withholds the no-matches claim when a per-kind status failed under a healthy rollup', () => {
    // The definitive sentence ("every consulted provider answered successfully … none had
    // this title") is exactly what this response contradicts.
    const kindFailed = museSearchResponse(response({
      results: [],
      providers: [provider({ status: 'ok', result_count: 0, kinds: [providerKind({ status: 'error', result_count: 0, provider_returned: 0, error: 'timeout' })] })],
    }));
    expect(searchOutcome({ ...base, parsed: kindFailed }).state).toBe('incomplete-empty');
  });

  it('withholds the no-matches claim when a status cannot be interpreted', () => {
    // An unknown status is not a success. It is also not a coverage loss — we have not
    // observed that anything was lost — so it earns the weaker sentence, not the stronger one.
    const unknown = museSearchResponse(response({
      results: [],
      providers: [provider({ status: 'rate_limited', result_count: 0, kinds: [providerKind({ status: 'ok', result_count: 0, provider_returned: 0 })] })],
    }));
    const out = searchOutcome({ ...base, parsed: unknown });
    expect(out.state).toBe('indeterminate-empty');
    expect(out.state).not.toBe('no-matches');
    expect(out.state).not.toBe('incomplete-empty');
  });

  it('still reports caveats alongside a non-empty result list', () => {
    // The dangerous case is not the empty one — it is five results that LOOK like an answer
    // while a whole media kind is missing.
    const out = searchOutcome({ ...base, parsed: museSearchResponse(response({ complete: false, uncovered_kinds: ['series'] })) });
    expect(out.state).toBe('results');
    expect(out.caveats).toContainEqual({ code: 'uncovered-kinds', kinds: ['series'], complete: false });
  });

  it('emits no caveats for a healthy complete search', () => {
    expect(searchOutcome(base).caveats).toEqual([]);
  });
});

describe('searchCaveats', () => {
  it('raises coverage loss from complete:false even with no kinds named', () => {
    const c = searchCaveats(response({ complete: false, uncovered_kinds: [] }));
    expect(c).toEqual([{ code: 'uncovered-kinds', kinds: [], complete: false }]);
  });

  it('collects the provider’s own error messages verbatim', () => {
    // Provider-level error with nothing notable at the kind level — the provider-level
    // sentence is the only signal, so it is the one reported.
    const c = searchCaveats(response({
      providers: [provider({ status: 'error', kinds: [providerKind({ status: 'ok', error: 'tmdb 401 unauthorized' })] })],
    }));
    expect(c).toContainEqual({ code: 'provider-error', provider: 'tmdb', messages: ['tmdb 401 unauthorized'] });
  });

  it('reports a failed provider that gave no message without inventing one', () => {
    const c = searchCaveats(response({ providers: [provider({ status: 'error', kinds: [providerKind({ status: 'ok' })] })] }));
    expect(c).toContainEqual({ code: 'provider-error', provider: 'tmdb', messages: [] });
  });

  it('reads the PER-KIND status, not just the provider rollup', () => {
    // The finding this guards: a kind that errored under a provider-level `ok` rendered as
    // "every consulted provider answered successfully", a definitive claim contradicted by
    // the very response it came from. Today's server rolls a provider up to `partial` when a
    // kind errors — the page must not DEPEND on that rollup being right.
    const c = searchCaveats(response({
      providers: [provider({ status: 'ok', kinds: [providerKind({ status: 'error', error: 'tvdb timeout' })] })],
    }));
    expect(c).toContainEqual({ code: 'kind-error', provider: 'tmdb', kind: 'movie', message: 'tvdb timeout' });
  });

  it('reports a per-kind partial under a healthy provider rollup', () => {
    const c = searchCaveats(response({
      providers: [provider({ status: 'ok', kinds: [providerKind({ status: 'partial', error: null })] })],
    }));
    expect(c).toContainEqual({ code: 'kind-partial', provider: 'tmdb', kind: 'movie', message: null });
  });

  it('does not print the same failure twice when the rollup agrees with the kind', () => {
    // The realistic payload: provider rolled up to `partial` because its one kind errored.
    // The kind-level sentence names the kind and carries its message, so it is strictly more
    // specific — the provider-level one is suppressed rather than duplicated.
    const c = searchCaveats(response({
      providers: [provider({ status: 'partial', kinds: [providerKind({ status: 'error', error: 'boom' })] })],
    }));
    expect(c).toContainEqual({ code: 'kind-error', provider: 'tmdb', kind: 'movie', message: 'boom' });
    expect(c.filter(x => x.code === 'provider-partial' || x.code === 'provider-error')).toEqual([]);
  });

  it('raises a caveat for a status it does not recognize, at either level', () => {
    const kindLevel = searchCaveats(response({
      providers: [provider({ status: 'ok', kinds: [providerKind({ status: 'rate_limited' })] })],
    }));
    expect(kindLevel).toContainEqual({ code: 'unknown-status', scope: 'kind', provider: 'tmdb', kind: 'movie', status: 'rate_limited' });

    const providerLevel = searchCaveats(response({ providers: [provider({ status: 'degraded_upstream' })] }));
    expect(providerLevel).toContainEqual({ code: 'unknown-status', scope: 'provider', provider: 'tmdb', kind: null, status: 'degraded_upstream' });
  });

  it('reports an unrecognized PROVIDER status even when a kind already reported one', () => {
    // Two different facts, not a rollup of one another.
    const c = searchCaveats(response({
      providers: [provider({ status: 'weird_provider', kinds: [providerKind({ status: 'weird_kind' })] })],
    }));
    expect(c).toContainEqual({ code: 'unknown-status', scope: 'kind', provider: 'tmdb', kind: 'movie', status: 'weird_kind' });
    expect(c).toContainEqual({ code: 'unknown-status', scope: 'provider', provider: 'tmdb', kind: null, status: 'weird_provider' });
  });

  it('raises no caveat for the four statuses it does understand', () => {
    for (const status of ['ok', 'not_consulted']) {
      expect(searchCaveats(response({ providers: [provider({ status, kinds: [providerKind({ status })] })] }))).toEqual([]);
    }
  });

  it('flags a truncated response that carries no results as self-contradictory', () => {
    const c = searchCaveats(response({
      results: [],
      providers: [provider({ kinds: [providerKind({ truncated: true, result_count: 0, provider_returned: 137 })] })],
    }));
    expect(c).toContainEqual({ code: 'contradictory-empty' });
    // Not raised when there ARE results — truncation is then an ordinary, consistent fact.
    const consistent = searchCaveats(response({
      providers: [provider({ kinds: [providerKind({ truncated: true, result_count: 40, provider_returned: 137 })] })],
    }));
    expect(consistent.some(x => x.code === 'contradictory-empty')).toBe(false);
  });

  it('surfaces truncation with the numbers the response actually carried', () => {
    const c = searchCaveats(response({
      providers: [provider({ kinds: [providerKind({ truncated: true, result_count: 40, provider_returned: 137, limit: 40 })] })],
    }));
    expect(c).toContainEqual({ code: 'truncated', provider: 'tmdb', kind: 'movie', shown: 40, providerReturned: 137, limit: 40 });
  });

  it('does not treat truncation as coverage loss', () => {
    // Truncation means there were MORE matches, so it must never push a result set into the
    // "this search did not complete" copy that a failed provider or an uncovered kind does.
    //
    // The fixture is deliberately CONTRIVED — zero results alongside a truncated kind is not
    // something the endpoint can produce. That is the point: an empty list is the only place
    // the classification is observable, so pinning the branch requires constructing it. An
    // earlier version of this test asserted through a NON-empty list, where the state is
    // 'results' regardless of the caveats — it passed against a mutant that classified every
    // caveat as coverage loss, i.e. it tested nothing.
    const truncatedEmpty = museSearchResponse(response({
      results: [],
      providers: [provider({
        result_count: 0,
        kinds: [providerKind({ truncated: true, result_count: 0, provider_returned: 137, limit: 40 })],
      })],
    }));
    const out = searchOutcome({ submitted: true, loading: false, degraded: false, parsed: truncatedEmpty });
    expect(out.caveats.map(c => c.code)).toEqual(['truncated', 'contradictory-empty']);
    // NOT coverage loss — that reasoning stands: truncation means there were MORE matches, so
    // it cannot have removed titles from the list.
    expect(out.state).not.toBe('incomplete-empty');
    // But it must not produce a confident negative either: zero results plus truncation is
    // contradictory, and the page does not resolve a contradiction in favour of the definitive
    // reading.
    expect(out.state).toBe('indeterminate-empty');
    expect(out.state).not.toBe('no-matches');

    // And with real results present, truncation is reported without downgrading the state.
    const truncated = museSearchResponse(response({
      providers: [provider({ kinds: [providerKind({ truncated: true, result_count: 40, provider_returned: 137 })] })],
    }));
    expect(searchOutcome({ submitted: true, loading: false, degraded: false, parsed: truncated }).state).toBe('results');
  });

  it('does not raise a caveat for a provider that was simply not consulted', () => {
    // `not_consulted` means the kind filter excluded it — that is the user's choice, not a
    // failure, and flagging it would cry wolf on every filtered search.
    const c = searchCaveats(response({ providers: [provider({ status: 'not_consulted', searched_kinds: [], kinds: [] })] }));
    expect(c).toEqual([]);
  });
});

describe('ownershipState — three answers, and null is not one of the other two', () => {
  it('reports held / not-held / unknown for the three legal values', () => {
    expect(ownershipState({ in_library: true })).toBe('held');
    expect(ownershipState({ in_library: false })).toBe('not-held');
    expect(ownershipState({ in_library: null })).toBe('unknown');
  });

  it('never collapses null into not-held', () => {
    // The failure mode this guards: `!r.in_library` reads null as "you do not have this",
    // which offers a Request button for a title the operator may already own. The assertion
    // is written as an inequality as well as an equality so that collapsing the branches
    // fails here even if 'unknown' were renamed.
    expect(ownershipState({ in_library: null })).not.toBe('not-held');
    expect(ownershipState({ in_library: null })).not.toBe(ownershipState({ in_library: false }));
  });

  it('lands anything non-boolean that survived the parser on the least-claiming state', () => {
    // Defence in depth: the parser rejects these, but a decision function about ownership
    // must not depend on that to avoid asserting something false.
    expect(ownershipState({ in_library: undefined as never })).toBe('unknown');
    expect(ownershipState({ in_library: 'true' as never })).toBe('unknown');
    expect(ownershipState({ in_library: 0 as never })).toBe('unknown');
  });
});

describe('providerCatalogState — an empty provider array means four different things', () => {
  it('says not-yet-searched ONLY when nothing was searched', () => {
    expect(providerCatalogState('idle', 0)).toBe('idle');
    // The finding: all three of these previously rendered "until a search runs there is
    // nothing to list", which asserts that no search had run. One had failed, one had
    // returned a body we could not read, and one had completed.
    expect(providerCatalogState('degraded', 0)).not.toBe('idle');
    expect(providerCatalogState('unrecognized', 0)).not.toBe('idle');
    expect(providerCatalogState('no-matches', 0)).not.toBe('idle');
  });

  it('routes a failed or unreadable search to its own state', () => {
    expect(providerCatalogState('loading', 0)).toBe('loading');
    expect(providerCatalogState('degraded', 0)).toBe('degraded');
    expect(providerCatalogState('unrecognized', 0)).toBe('unrecognized');
  });

  it('treats an empty provider array from a COMPLETED search as its own, anomalous state', () => {
    // The endpoint reports every provider it knows about on every search, so this is not a
    // pending state and not a normal one.
    for (const state of ['no-matches', 'incomplete-empty', 'indeterminate-empty', 'results'] as const) {
      expect(providerCatalogState(state, 0)).toBe('no-providers-reported');
    }
  });

  it('lists providers whenever a completed search reported any', () => {
    for (const state of ['no-matches', 'incomplete-empty', 'indeterminate-empty', 'results'] as const) {
      expect(providerCatalogState(state, 2)).toBe('providers');
    }
  });
});

describe('catalogState — null is not "not in the catalog"', () => {
  it('reports the three answers', () => {
    expect(catalogState({ in_catalog: true })).toBe('in-catalog');
    expect(catalogState({ in_catalog: false })).toBe('not-in-catalog');
    expect(catalogState({ in_catalog: null })).toBe('unknown');
  });

  it('never lets an unresolved hit earn the "In catalog" claim', () => {
    // The badge is rendered on `=== 'in-catalog'` only. A truthiness test would render null
    // exactly like false — safe today only because there is no negative badge, which is not a
    // property this function may depend on.
    expect(catalogState({ in_catalog: null })).not.toBe('in-catalog');
    expect(catalogState({ in_catalog: undefined as never })).not.toBe('in-catalog');
    expect(catalogState({ in_catalog: null })).not.toBe(catalogState({ in_catalog: false }));
  });
});

describe('ownershipReason — one sentence per resolution, never shared', () => {
  const reasons = ['no_indexed_identifier', 'ambiguous_rows', 'contradicted'].map(resolution =>
    ownershipReason({ resolution }),
  );

  it('gives each known unresolved case its own wording', () => {
    // Each one sends an operator somewhere different: nothing to fix / dedupe the catalog /
    // a bad identifier. Shared copy would erase what the field was added to carry.
    expect(new Set(reasons).size).toBe(3);
  });

  it('says NOT-CHECKED for no_indexed_identifier, and does not say it for the others', () => {
    const notChecked = ownershipReason({ resolution: 'no_indexed_identifier' });
    expect(notChecked).toMatch(/could not check/i);
    expect(notChecked).toMatch(/not a finding that you do not own it/i);
    expect(ownershipReason({ resolution: 'ambiguous_rows' })).not.toMatch(/could not check/i);
    expect(ownershipReason({ resolution: 'ambiguous_rows' })).toMatch(/share/i);
    expect(ownershipReason({ resolution: 'contradicted' })).toMatch(/disagrees/i);
  });

  it('echoes an unrecognized resolution verbatim instead of mapping it onto a known one', () => {
    const text = ownershipReason({ resolution: 'some_future_state' });
    expect(text).toContain('some_future_state');
    expect(reasons).not.toContain(text);
  });
});

describe('isCheckedNegative — the two negatives that used to look identical', () => {
  it('is true only for a definite false that came from an actual lookup', () => {
    expect(isCheckedNegative({ in_library: false, resolution: 'absent' })).toBe(true);
  });

  it('is false when nothing was looked up', () => {
    // The whole reason `resolution` exists. `no_indexed_identifier` arrives with a null
    // in_library today, but the guard is on BOTH fields so a future false+unchecked pairing
    // still cannot be announced as "checked, not found".
    expect(isCheckedNegative({ in_library: null, resolution: 'no_indexed_identifier' })).toBe(false);
    expect(isCheckedNegative({ in_library: false, resolution: 'no_indexed_identifier' })).toBe(false);
  });

  it('is false for held, unknown, and every non-absent resolution', () => {
    expect(isCheckedNegative({ in_library: true, resolution: 'settled' })).toBe(false);
    expect(isCheckedNegative({ in_library: false, resolution: 'settled' })).toBe(false);
    expect(isCheckedNegative({ in_library: null, resolution: 'ambiguous_rows' })).toBe(false);
    expect(isCheckedNegative({ in_library: null, resolution: 'absent' })).toBe(false);
  });
});

describe('parseQualityProfileId', () => {
  it('accepts a positive integer', () => {
    expect(parseQualityProfileId('1')).toBe(1);
    expect(parseQualityProfileId('  7 ')).toBe(7);
  });

  it('rejects everything Muse would 400 on', () => {
    // The button is disabled on null, so each of these keeps a request from being fired at
    // all rather than being sent for Muse to reject.
    for (const bad of ['', '   ', '0', '-1', '1.5', '1e3', 'abc', '1abc', '٣']) {
      expect(parseQualityProfileId(bad)).toBeNull();
    }
  });
});
