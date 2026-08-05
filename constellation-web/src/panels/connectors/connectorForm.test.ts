// RMCP-13 (TERM-624): the pure helpers behind the Connectors editors.
//
// The cases that matter here are the ones where a wrong answer is a SECURITY answer: a scope
// summary that reads as permissive when the client reaches nothing, and a redirect-URI hint
// that waves through something the server will (rightly) refuse.
import { describe, it, expect } from 'vitest';
import { RMCP_TOOLS, RmcpError } from '../../lib/rmcpContract';
import {
  accountRefusal,
  accountSuggestions,
  assignmentIncomplete,
  serverUnassignableReason,
  pageCount,
  pageSlice,
  parseLines,
  patternHint,
  redirectUriHint,
  redirectUriHints,
  sameSet,
  scopeSummary,
} from './connectorForm';

describe('parseLines', () => {
  it('trims and drops blank lines', () => {
    expect(parseLines('  a \n\n b\n')).toEqual(['a', 'b']);
  });

  it('returns an empty list for an empty textarea (not [""])', () => {
    expect(parseLines('')).toEqual([]);
    expect(parseLines('   \n  ')).toEqual([]);
  });
});

describe('redirectUriHint', () => {
  it('accepts absolute https', () => {
    expect(redirectUriHint('https://example.invalid/callback')).toBeNull();
  });

  it('accepts an RFC 8252 loopback IP literal over http', () => {
    expect(redirectUriHint('http://127.0.0.1:7777/callback')).toBeNull();
    expect(redirectUriHint('http://[::1]:7777/callback')).toBeNull();
  });

  it('rejects non-loopback http — the headline case', () => {
    expect(redirectUriHint('http://example.invalid/callback')).toMatch(/loopback/);
  });

  it('rejects a loopback NAME, which is not an IP literal', () => {
    expect(redirectUriHint('http://localhost:7777/callback')).toMatch(/loopback/);
  });

  it('rejects fragments and wildcards', () => {
    expect(redirectUriHint('https://example.invalid/cb#frag')).toMatch(/fragment/);
    expect(redirectUriHint('https://example.invalid/*')).toMatch(/wildcard/);
  });

  it('rejects a relative or unparseable URI', () => {
    expect(redirectUriHint('/callback')).toMatch(/absolute/);
  });

  it('rejects a non-http scheme', () => {
    expect(redirectUriHint('ftp://example.invalid/cb')).toMatch(/only https/);
  });

  it('reports only the problematic URIs', () => {
    expect(redirectUriHints(['https://example.invalid/a', 'http://example.invalid/b'])).toEqual([
      { uri: 'http://example.invalid/b', hint: expect.stringMatching(/loopback/) },
    ]);
  });
});

describe('scopeSummary — describes assignment, never claims reach', () => {
  const enabled = { enabled: true, toolGroupIds: ['g'], namespaces: ['n'] };

  it('names a missing group or server as UNASSIGNED, not as a reachability verdict', () => {
    // Reach is the server's answer (the resolved preview). A local claim about it — even a
    // conservative one — diverges silently the day the server's rules grow.
    expect(scopeSummary({ ...enabled, toolGroupIds: [] })).toContain('no tool groups assigned');
    expect(scopeSummary({ ...enabled, namespaces: [] })).toContain('no servers assigned');
    expect(scopeSummary({ ...enabled, toolGroupIds: [] })).not.toMatch(/reach/i);
  });

  it('marks a disabled connector without asserting what it can call', () => {
    expect(scopeSummary({ ...enabled, enabled: false })).toMatch(/^disabled/);
  });

  it('never renders an unassigned connector as unrestricted', () => {
    const summary = scopeSummary({ enabled: true, toolGroupIds: [], namespaces: [] });
    expect(summary).not.toMatch(/all|every|unrestricted/i);
    expect(summary).toBe('no tool groups assigned · no servers assigned');
  });

  it('summarises a complete assignment by count', () => {
    expect(scopeSummary({ enabled: true, toolGroupIds: ['a', 'b'], namespaces: ['n'] })).toBe('2 groups · 1 server');
  });
});

describe('assignmentIncomplete — a presentation flag about the record, not about access', () => {
  it('is true when groups, servers, or the enabled flag are missing', () => {
    expect(assignmentIncomplete({ enabled: true, toolGroupIds: [], namespaces: ['n'] })).toBe(true);
    expect(assignmentIncomplete({ enabled: true, toolGroupIds: ['g'], namespaces: [] })).toBe(true);
    expect(assignmentIncomplete({ enabled: false, toolGroupIds: ['g'], namespaces: ['n'] })).toBe(true);
  });

  it('is false for a complete, enabled assignment', () => {
    expect(assignmentIncomplete({ enabled: true, toolGroupIds: ['g'], namespaces: ['n'] })).toBe(false);
  });
});

describe('serverUnassignableReason — three states, three remedies', () => {
  it('permits an owned server', () => {
    expect(serverUnassignableReason({ ownedByMe: true, ownerName: 'me' })).toBeUndefined();
  });

  it('names the owner when there is one to ask', () => {
    expect(serverUnassignableReason({ ownedByMe: false, ownerName: 'someone-else' })).toContain('someone-else');
  });

  it('says UNCLAIMED when nobody owns it — not "you do not own this"', () => {
    // The store refuses an unclaimed namespace exactly as it refuses someone else's, but the
    // remedies differ: "ask the owner" is useless advice when there is no owner.
    const reason = serverUnassignableReason({ ownedByMe: false, ownerName: null })!;
    expect(reason).toMatch(/unclaimed/i);
    expect(reason).not.toMatch(/you do not own/i);
  });
});

describe('sameSet', () => {
  it('ignores order', () => {
    expect(sameSet(['a', 'b'], ['b', 'a'])).toBe(true);
  });

  it('detects a difference in length or membership', () => {
    expect(sameSet(['a'], ['a', 'b'])).toBe(false);
    expect(sameSet(['a'], ['b'])).toBe(false);
  });
});

describe('patternHint — a typing aid, never an authority', () => {
  it('accepts the three supported forms', () => {
    expect(patternHint('media_search')).toBeNull();
    expect(patternHint('media_*')).toBeNull();
    expect(patternHint('media::*')).toBeNull();
  });

  it('flags a mid-string wildcard, whitespace, and empties', () => {
    expect(patternHint('me*ia')).toMatch(/end/);
    expect(patternHint(' media')).toMatch(/whitespace/);
    expect(patternHint('')).toMatch(/empty/);
  });

  it('flags regex-ish characters rather than trying to interpret them', () => {
    expect(patternHint('^media.*$')).toMatch(/unsupported characters/);
  });

  it('describes bare * as ownership-dependent, not as invalid — only the server knows', () => {
    const hint = patternHint('*');
    expect(hint).toMatch(/operator-owned/);
    expect(hint).not.toMatch(/invalid/);
  });
});

describe('pageSlice / pageCount', () => {
  const rows = Array.from({ length: 55 }, (_, i) => i);

  it('slices the requested page', () => {
    expect(pageSlice(rows, 0, 25)).toHaveLength(25);
    expect(pageSlice(rows, 2, 25)).toEqual([50, 51, 52, 53, 54]);
  });

  it('treats a negative page as the first one rather than slicing from the end', () => {
    expect(pageSlice(rows, -1, 25)[0]).toBe(0);
  });

  it('reports at least one page for an empty list', () => {
    expect(pageCount(0, 25)).toBe(1);
    expect(pageCount(55, 25)).toBe(3);
  });
});

// ── TERM-647 ─────────────────────────────────────────────────────────────────────────────────

describe('accountSuggestions', () => {
  const row = (namespace: string, ownerName: string | null) => ({ namespace, ownerName });

  it('offers each known owner once, sorted', () => {
    expect(
      accountSuggestions([row('studio', 'studio-owner'), row('media', 'delegated-owner'), row('home', 'delegated-owner')]),
    ).toEqual(['delegated-owner', 'studio-owner']);
  });

  it('contributes nothing for an unclaimed namespace', () => {
    // `null` is "no ownership row exists", which names no account — distinct from an account
    // whose name happens to be unknown to this session.
    expect(accountSuggestions([row('lab', null)])).toEqual([]);
  });

  it('is empty rather than invented when the session sees no ownership', () => {
    // The empty case is the one that matters: there is no account-listing tool, so this is
    // routinely empty, and the field it feeds must still accept a typed name.
    expect(accountSuggestions([])).toEqual([]);
  });

  it('ignores a blank name and trims the rest', () => {
    expect(accountSuggestions([row('a', '  '), row('b', ' spaced ')])).toEqual(['spaced']);
  });
});

describe('accountRefusal', () => {
  it('re-words a create not_found in terms of the accounts, keeping the server text', () => {
    const m = accountRefusal(new RmcpError('not_found', RMCP_TOOLS.clientCreate, 'no such account'));
    expect(m).toMatch(/No such account/i);
    expect(m).toMatch(/disabled/i);
    expect(m).toContain('no such account');
    // The misleading generic wording must NOT be what an operator sees here.
    expect(m).not.toMatch(/revoked elsewhere/i);
  });

  it('re-words a create forbidden as an authority problem', () => {
    expect(accountRefusal(new RmcpError('forbidden', RMCP_TOOLS.clientCreate, 'nope'))).toMatch(
      /operator authority/i,
    );
  });

  it('declines everything else, so the shared wording still applies', () => {
    // Only the two kinds whose generic copy is actively wrong on this path are specialised.
    // `invalid` in particular carries the server's field-level details and must pass through.
    expect(accountRefusal(new RmcpError('invalid', RMCP_TOOLS.clientCreate, 'bad uri'))).toBeNull();
    expect(accountRefusal(new RmcpError('conflict', RMCP_TOOLS.clientCreate, 'stale'))).toBeNull();
    // ...and it is scoped to the create tool: a not_found from a READ really does mean the row
    // went away, and re-wording that as an account problem would be the same error inverted.
    expect(accountRefusal(new RmcpError('not_found', RMCP_TOOLS.clientUpdate, 'gone'))).toBeNull();
    expect(accountRefusal(new Error('boom'))).toBeNull();
    expect(accountRefusal(undefined)).toBeNull();
  });
});
