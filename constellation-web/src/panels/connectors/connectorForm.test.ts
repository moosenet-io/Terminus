// RMCP-13 (TERM-624): the pure helpers behind the Connectors editors.
//
// The cases that matter here are the ones where a wrong answer is a SECURITY answer: a scope
// summary that reads as permissive when the client reaches nothing, and a redirect-URI hint
// that waves through something the server will (rightly) refuse.
import { describe, it, expect } from 'vitest';
import {
  assignmentIncomplete,
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
