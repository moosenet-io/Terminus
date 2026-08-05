// TERM #654, review round 4 finding 2: the fix that stops a tool REFUSAL announcing SUCCESS was
// itself unpinned.
//
// `callTool` receives a refusal as an HTTP 200 carrying `{ok:false}` — a settled request — so the
// activity/toast layer called it a success while the panel showed the refusal inline. The fix
// passes an `isOk` classifier down to `withMutationResultEvent`. Nothing tested it, and it could
// not have: the account tests run in MOCK mode, where `callTool` returns through the fixture
// server and never reaches `AggregationClient.request` at all. Deleting the classifier would have
// left every one of them green.
//
// So this test drives the HTTP adapter with a stubbed `fetch` and observes the emitted mutation
// event directly. That is the only place the property is visible.
//
// jsdom because `resolveMode()` forces `mock` whenever `window` is undefined — the node default —
// which is precisely the reason the rest of the suite could never reach this path.
//
// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { onMutationResult } from './aggregationClient';
import type { MutationResultEvent } from './aggregationClient';
import { listClients, RmcpError } from './rmcpClient';

const realFetch = globalThis.fetch;

/** Answer the dispatch endpoint with one envelope, at HTTP 200 — the shape a REFUSAL takes. */
function respondWith(body: unknown) {
  globalThis.fetch = vi.fn(async () =>
    new Response(JSON.stringify(body), { status: 200, headers: { 'content-type': 'application/json' } }),
  ) as unknown as typeof fetch;
}

beforeEach(() => {
  (window as unknown as { __AGG_MODE__?: string }).__AGG_MODE__ = 'http';
});

afterEach(() => {
  globalThis.fetch = realFetch;
  delete (window as unknown as { __AGG_MODE__?: string }).__AGG_MODE__;
});

describe('a tool refusal is reported as a FAILED mutation, not a successful one', () => {
  it('emits ok:false for an {ok:false} envelope', async () => {
    const seen: MutationResultEvent[] = [];
    const off = onMutationResult(e => seen.push(e));
    respondWith({ ok: false, error: { code: 'conflict', message: "that is this deployment's last active operator" } });

    await expect(listClients()).rejects.toBeInstanceOf(RmcpError);
    off();

    const dispatches = seen.filter(e => e.path.includes('/rmcp/call'));
    expect(dispatches.length).toBe(1);
    // Mutation-verify: delete the `envelope => envelope?.ok === true` classifier passed by
    // `callTool` and this flips to `true` — the operator gets a green toast for a refused write.
    expect(dispatches[0].ok).toBe(false);
  });

  it('still emits ok:true for a genuine success, so the fix did not invert the report', async () => {
    const seen: MutationResultEvent[] = [];
    const off = onMutationResult(e => seen.push(e));
    respondWith({ ok: true, result: { clients: [] } });

    await expect(listClients()).resolves.toEqual([]);
    off();

    const dispatches = seen.filter(e => e.path.includes('/rmcp/call'));
    expect(dispatches.length).toBe(1);
    expect(dispatches[0].ok).toBe(true);
  });
});
