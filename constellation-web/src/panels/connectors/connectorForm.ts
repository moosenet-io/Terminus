// RMCP-13 (TERM-624): pure helpers for the Connectors editors. No React, no I/O — unit-tested
// in `connectorForm.test.ts`.
//
// EVERY CHECK IN THIS FILE IS A HINT, NOT A RULE. The server validates redirect URIs (RMCP-08)
// and tool-group patterns (RMCP-06) at write time and is the only authority on either. These
// functions exist so the operator sees the likely problem while typing instead of after a
// round trip; a value this file is happy with can still be refused, and that refusal is
// surfaced verbatim. Nothing here ever *permits* anything.
import type { RmcpClient } from '../../types/rmcp';

/** Split a textarea's contents into non-empty, trimmed lines. */
export function parseLines(text: string): string[] {
  return text
    .split('\n')
    .map(l => l.trim())
    .filter(l => l.length > 0);
}

/**
 * A likely problem with a redirect URI, or null if nothing stands out. Mirrors the RMCP-08
 * rule the server enforces: absolute HTTPS, or an RFC 8252 loopback redirect, and nothing else
 * — no fragments, no wildcards.
 */
export function redirectUriHint(uri: string): string | null {
  let parsed: URL;
  try {
    parsed = new URL(uri);
  } catch {
    return 'not an absolute URI';
  }
  if (parsed.hash) return 'fragments are not allowed';
  if (uri.includes('*')) return 'wildcards are not allowed';
  if (parsed.protocol === 'https:') return null;
  if (parsed.protocol === 'http:') {
    // RFC 8252 §7.3 — loopback IP literals only. A loopback NAME is not accepted, matching the
    // RFC's own reasoning (name resolution is attackable), and neither is any other host.
    const isLoopbackLiteral = parsed.hostname === '127.0.0.1' || parsed.hostname === '[::1]';
    return isLoopbackLiteral ? null : 'http is only allowed for a loopback IP literal redirect';
  }
  return 'only https, or http on a loopback IP literal, is accepted';
}

/** Per-line hints for a redirect-URI textarea, keyed by the URI. */
export function redirectUriHints(uris: string[]): { uri: string; hint: string }[] {
  return uris
    .map(uri => ({ uri, hint: redirectUriHint(uri) }))
    .filter((x): x is { uri: string; hint: string } => x.hint !== null);
}

/**
 * The one-line answer to "what does this connector reach?", used in the list so an unscoped
 * client is obvious without opening it.
 *
 * Absence is denial: a client with no groups OR no namespaces reaches NOTHING, and this says so
 * in those words. It deliberately never renders as "all" or "unrestricted" — the reading that
 * would make an unscoped client look powerful is the one that gets someone hurt.
 */
export function scopeSummary(client: Pick<RmcpClient, 'toolGroupIds' | 'namespaces' | 'enabled'>): string {
  if (!client.enabled) return 'disabled — reaches nothing';
  if (client.toolGroupIds.length === 0 && client.namespaces.length === 0) {
    return 'unscoped — reaches nothing';
  }
  if (client.toolGroupIds.length === 0) return 'no tool groups — reaches nothing';
  if (client.namespaces.length === 0) return 'no servers — reaches nothing';
  const g = `${client.toolGroupIds.length} group${client.toolGroupIds.length === 1 ? '' : 's'}`;
  const n = `${client.namespaces.length} server${client.namespaces.length === 1 ? '' : 's'}`;
  return `${g} × ${n}`;
}

/** Whether a client is scoped such that the server could return anything at all. Used to skip a
 *  pointless resolve call and render the explicit "reaches nothing" state instead. */
export function reachesNothing(client: Pick<RmcpClient, 'toolGroupIds' | 'namespaces' | 'enabled'>): boolean {
  return !client.enabled || client.toolGroupIds.length === 0 || client.namespaces.length === 0;
}

/** Shallow set equality for the multi-selects, so "Save" can be disabled when nothing changed
 *  (and, more importantly, so an unchanged form never issues a write that could conflict). */
export function sameSet(a: string[], b: string[]): boolean {
  if (a.length !== b.length) return false;
  const set = new Set(a);
  return b.every(x => set.has(x));
}

/** A likely problem with a tool-group pattern, or null. The vocabulary is RMCP-06's: an exact
 *  name, a trailing-`*` prefix, or `<namespace>::*`. Same caveat as the URI hint — the server
 *  decides; this only saves a round trip. `*` alone is flagged as operator-only rather than
 *  invalid, because whether it is allowed depends on ownership, which only the server knows. */
export function patternHint(pattern: string): string | null {
  if (pattern.length === 0) return 'empty pattern';
  if (pattern !== pattern.trim()) return 'leading or trailing whitespace';
  if (pattern === '*') return 'matches everything — accepted only for an operator-owned group';
  // Character check first: for something regex-shaped (`^media.*$`) "unsupported characters" is
  // the useful diagnosis, while "a wildcard may only appear at the end" would send the operator
  // off to move a `*` that was never the problem.
  if (/[^A-Za-z0-9_:*-]/.test(pattern)) return 'unsupported characters — no regex, no negation';
  const star = pattern.indexOf('*');
  if (star !== -1 && star !== pattern.length - 1) return 'a wildcard may only appear at the end';
  return null;
}

/** Slice one page out of a resolved list. Kept pure (and tested) so the preview's paging cannot
 *  drift from the count it reports. */
export function pageSlice<T>(rows: T[], page: number, pageSize: number): T[] {
  const start = Math.max(0, page) * pageSize;
  return rows.slice(start, start + pageSize);
}

/** Total page count for `rows.length` items, minimum 1 (an empty list is one empty page, not
 *  zero pages — a zero would render "page 1 of 0"). */
export function pageCount(total: number, pageSize: number): number {
  return Math.max(1, Math.ceil(total / pageSize));
}
