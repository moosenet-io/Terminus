// RMCP-13 (TERM-624): pure helpers for the Connectors editors. No React, no I/O — unit-tested
// in `connectorForm.test.ts`.
//
// EVERY CHECK IN THIS FILE IS A HINT, NOT A RULE. The server validates redirect URIs (RMCP-08)
// and tool-group patterns (RMCP-06) at write time and is the only authority on either. These
// functions exist so the operator sees the likely problem while typing instead of after a
// round trip; a value this file is happy with can still be refused, and that refusal is
// surfaced verbatim. Nothing here ever *permits* anything.
import type { RmcpClient, RmcpServer } from '../../types/rmcp';

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
 * A one-line DESCRIPTION of what a connector is assigned, for the list column.
 *
 * Note what it deliberately is not: a claim about what the connector can reach. That question is
 * answered only by the server, in the resolved preview (review round 1 — the same reasoning that
 * removed the preview's local pre-check applies to any local reachability claim, including a
 * conservative one, because a claim that is right today diverges silently when the server's rules
 * grow). So this reports assignment — "no tool groups assigned" — and the detail view's preview
 * reports reach.
 *
 * It still never renders an unassigned connector as "all" or "unrestricted": that reading is the
 * one that gets someone hurt, and no wording here should invite it.
 */
export function scopeSummary(client: Pick<RmcpClient, 'toolGroupIds' | 'namespaces' | 'enabled'>): string {
  const parts: string[] = [];
  if (client.toolGroupIds.length === 0) parts.push('no tool groups assigned');
  else parts.push(`${client.toolGroupIds.length} group${client.toolGroupIds.length === 1 ? '' : 's'}`);
  if (client.namespaces.length === 0) parts.push('no servers assigned');
  else parts.push(`${client.namespaces.length} server${client.namespaces.length === 1 ? '' : 's'}`);
  const summary = parts.join(' · ');
  return client.enabled ? summary : `disabled · ${summary}`;
}

/** Whether a summary describes an INCOMPLETE assignment (missing groups, missing servers, or
 *  disabled) — used only to colour the list cell as needing attention. It is a presentation
 *  decision about the connector's own record, not a statement about what it can reach. */
export function assignmentIncomplete(
  client: Pick<RmcpClient, 'toolGroupIds' | 'namespaces' | 'enabled'>,
): boolean {
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

/**
 * Why a server cannot be assigned by this session, or null if it can.
 *
 * Two refusals with different remedies, so they get different words: a namespace owned by someone
 * else needs that owner's agreement, while an UNCLAIMED one (no ownership row at all) cannot be
 * attached by anybody until it is claimed. The real store refuses both — its `set_client_namespaces`
 * INNER JOINs `rmcp_server_owner`, and its own comment is explicit that "nobody has claimed this
 * server" must never read as "everyone may reach it". Rendering the unclaimed case as "you do not
 * own this" would send an operator to ask a person who does not exist.
 */
export function serverUnassignableReason(server: Pick<RmcpServer, 'ownedByMe' | 'ownerName'>): string | undefined {
  if (server.ownedByMe) return undefined;
  return server.ownerName === null
    ? 'unclaimed server — no owner, so it cannot be assigned to anything'
    : `owned by ${server.ownerName} — not yours to assign`;
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
