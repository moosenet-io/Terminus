// MACT-06 (MUSE-126): pure-function coverage for the Maestro Activity stat-tile formatters.
// Every case here is chosen to pin the honesty rule this file exists to enforce — see
// tileFormat.ts's top comment for the three states, and the required-test note in this item's
// own TEST PLAN: "a genuine 0 renders as 0, not as not-reported" and "an H2 placeholder never
// shows 0" must be DISTINGUISHABLE, not just individually true.
import { describe, it, expect } from 'vitest';
import {
  DEGRADED_DETAIL_MAX_LEN,
  MAESTRO_SEAM_LABEL,
  formatCount,
  formatModuleHealth,
  formatMuseHealth,
  formatRelativeTimestamp,
  formatSubsystemWiring,
  tileStateFromSection,
  truncateDetail,
} from './tileFormat';

describe('formatCount — a genuine 0 is a value, never re-dashed to "not reported"', () => {
  it('renders null as "—"', () => {
    expect(formatCount(null)).toBe('—');
  });
  it('renders undefined as "—"', () => {
    expect(formatCount(undefined)).toBe('—');
  });
  it('renders NaN/Infinity as "—" (never a garbage numeral)', () => {
    expect(formatCount(NaN)).toBe('—');
    expect(formatCount(Infinity)).toBe('—');
  });
  it('renders a genuine 0 as "0" — THE case this item exists to prove', () => {
    expect(formatCount(0)).toBe('0');
  });
  it('renders a positive count verbatim', () => {
    expect(formatCount(1842)).toBe('1842');
  });
});

describe('formatRelativeTimestamp — null/unparsable render "—", never "0m ago"', () => {
  const now = Date.parse('2026-08-01T12:00:00Z');

  it('renders null as "—"', () => {
    expect(formatRelativeTimestamp(null, now)).toBe('—');
  });
  it('renders undefined as "—"', () => {
    expect(formatRelativeTimestamp(undefined, now)).toBe('—');
  });
  it('renders an unparsable string as "—"', () => {
    expect(formatRelativeTimestamp('not-a-date', now)).toBe('—');
  });
  it('renders under a minute as "just now"', () => {
    expect(formatRelativeTimestamp(new Date(now - 30_000).toISOString(), now)).toBe('just now');
  });
  it('renders minutes ago', () => {
    expect(formatRelativeTimestamp(new Date(now - 45 * 60_000).toISOString(), now)).toBe('45m ago');
  });
  it('renders hours ago', () => {
    expect(formatRelativeTimestamp(new Date(now - 5 * 3_600_000).toISOString(), now)).toBe('5h ago');
  });
  it('renders days ago', () => {
    expect(formatRelativeTimestamp(new Date(now - 3 * 86_400_000).toISOString(), now)).toBe('3d ago');
  });
});

describe('formatMuseHealth — Muse\'s own {status, db} vocabulary, unclassified verbatim', () => {
  it('status ok + db up -> success tone', () => {
    const r = formatMuseHealth({ status: 'ok', db: 'up' });
    expect(r.tone).toBe('success');
    expect(r.text).toBe('ok · db up');
  });
  it('db down is a WARNING tone, not a degrade -- the fetch itself still succeeded (200)', () => {
    const r = formatMuseHealth({ status: 'ok', db: 'down' });
    expect(r.tone).toBe('warning');
    expect(r.text).toBe('ok · db down');
  });
  it('an unrecognised db value renders verbatim + "(unrecognised)", never coerced', () => {
    const r = formatMuseHealth({ status: 'ok', db: 'unknown-value' });
    expect(r.text).toContain('unknown-value (unrecognised)');
    expect(r.tone).toBe('tertiary');
  });
  it('an unrecognised status renders verbatim + "(unrecognised)"', () => {
    const r = formatMuseHealth({ status: 'degraded', db: 'up' });
    expect(r.text).toContain('degraded (unrecognised)');
  });
});

describe('formatSubsystemWiring — a compact summary of the SAME /api/subsystems payload', () => {
  it('counts live case-insensitively against the total', () => {
    const r = formatSubsystemWiring([{ state: 'live' }, { state: 'LIVE' }, { state: 'seam' }, { state: 'unmounted' }]);
    expect(r.text).toBe('2 live of 4');
  });
  it('an empty list is "0 live of 0" -- a fact from a successful empty response', () => {
    expect(formatSubsystemWiring([]).text).toBe('0 live of 0');
  });
});

describe('formatModuleHealth — "N up of M" from the shell\'s existing GET /api/health', () => {
  it('counts available entries against the total', () => {
    const r = formatModuleHealth([
      { available: true }, { available: true }, { available: false }, { available: true },
    ]);
    expect(r.text).toBe('3 up of 4');
  });
  it('all-down still renders a real 0, not a dash', () => {
    expect(formatModuleHealth([{ available: false }, { available: false }]).text).toBe('0 up of 2');
  });
});

describe('tileStateFromSection — loading/degraded/ready classification', () => {
  it('loading section -> loading state, formatter never called', () => {
    let called = false;
    const state = tileStateFromSection(
      { data: null, loading: true, degraded: false },
      () => { called = true; return { text: 'x' }; },
    );
    expect(state).toEqual({ kind: 'loading' });
    expect(called).toBe(false);
  });
  it('degraded section -> degraded state with the detail', () => {
    const state = tileStateFromSection(
      { data: null, loading: false, degraded: { detail: 'HTTP 401 for /stats' } },
      () => ({ text: 'unused' }),
    );
    expect(state).toEqual({ kind: 'degraded', detail: 'HTTP 401 for /stats' });
  });
  it('a null body with no degraded flag is still treated as degraded (never dereferenced)', () => {
    const state = tileStateFromSection<{ n: number }>(
      { data: null, loading: false, degraded: false },
      d => ({ text: formatCount(d.n) }),
    );
    expect(state.kind).toBe('degraded');
  });
  it('ready section -> value state with the formatted text and tone', () => {
    const state = tileStateFromSection(
      { data: { n: 0 }, loading: false, degraded: false },
      d => ({ text: formatCount(d.n), tone: 'success' as const }),
    );
    expect(state).toEqual({ kind: 'value', text: '0', tone: 'success' });
  });
});

describe('truncateDetail — shortens a degrade cause for the tile\'s VISIBLE line', () => {
  it('returns a short string unchanged', () => {
    expect(truncateDetail('HTTP 401')).toBe('HTTP 401');
  });
  it('returns a string exactly at the bound unchanged', () => {
    const exact = 'x'.repeat(DEGRADED_DETAIL_MAX_LEN);
    expect(truncateDetail(exact)).toBe(exact);
    expect(truncateDetail(exact).length).toBe(DEGRADED_DETAIL_MAX_LEN);
  });
  it('ellipsizes a string over the bound, staying at or under the bound', () => {
    const long = 'HTTP 401 for /api/requests/queue (unauthenticated)';
    const r = truncateDetail(long);
    expect(r.length).toBeLessThanOrEqual(DEGRADED_DETAIL_MAX_LEN);
    expect(r.endsWith('…')).toBe(true);
    expect(long.startsWith(r.slice(0, -1))).toBe(true);
  });
  it('never returns the full untruncated string once it exceeds the bound', () => {
    const long = 'HTTP 401 for /api/requests/queue (unauthenticated, CONSTELLATION_MUSE_TOKEN unset)';
    expect(truncateDetail(long)).not.toBe(long);
  });
});

describe('MAESTRO_SEAM_LABEL — the fixed H2 placeholder text', () => {
  it('is the literal seam wording, never "0" and never empty', () => {
    expect(MAESTRO_SEAM_LABEL).toBe('requires Maestro — not deployed');
    expect(MAESTRO_SEAM_LABEL).not.toBe('0');
    expect(MAESTRO_SEAM_LABEL.length).toBeGreaterThan(0);
  });
});
