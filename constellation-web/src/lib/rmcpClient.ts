// RMCP-13 (TERM-624): the Connectors page's data layer.
//
// ONE PATH TO THE BACKEND, ONE PATH TO THE TOOLS.
// ------------------------------------------------------------------------------------------
// Every call in this file goes through `getAggregationClient().request('terminus', …)` (the
// app's single backend seam — see `aggregationClient.ts`'s module doc and its grep-enforced
// "nothing else calls fetch" test) and lands on ONE endpoint, `POST /api/terminus/rmcp/call`,
// which dispatches the named `rmcp_*` Terminus tool. That is deliberately the same door the CLI
// uses: there is no second REST surface for connector administration and no direct DB access
// from the web layer. Adding one later would let the GUI and the CLI drift into two different
// authorization stories, which is exactly the failure the S132 spec is written against.
//
// AUTHORIZATION IS NOT HERE.
// ------------------------------------------------------------------------------------------
// Nothing in this file (or in the panels above it) decides what a caller may see or do. The
// server scopes every read to the caller's ownership (RMCP-12) and refuses every unauthorized
// write. `RmcpClient.editable` and `RmcpServer.ownedByMe` exist only so the UI can avoid
// offering a control that would 403 — they are a courtesy, never an enforcement point. A
// delegated owner who edits the DOM gets a refusal from the server, unchanged.
//
// RESOLUTION IS NOT HERE EITHER.
// ------------------------------------------------------------------------------------------
// `resolveClientScope` and `previewGroup` ASK THE SERVER. There is no pattern matcher in this
// module and there must never be one: the preview's entire value is that it is the same
// answer `tools/list` and `tools/call` will give (RMCP-07's single `effective(...)`), and a
// second implementation in TypeScript is how a UI starts confidently lying about access.
//
// BACKEND READINESS (read this before concluding the page is broken).
// ------------------------------------------------------------------------------------------
// The `rmcp_*` tools land in RMCP-05/06/07/08/11/12, in parallel with this item. Until the
// dispatch endpoint exists, every call here resolves to the `tool_unavailable` error code and
// the page renders an explanatory "not live yet" state — the same posture `ActivityPanel` took
// against CONST-26's endpoint. It never renders an error page, and it never invents data.
import { getAggregationClient, resolveMode } from './aggregationClient';
import { rmcpFixtureCall } from './rmcpFixtures';
import { RMCP_TOOLS, RmcpError } from './rmcpContract';
import type { RmcpErrorKind, RmcpToolName } from './rmcpContract';
import type {
  RmcpClient,
  RmcpClientCreated,
  RmcpGroupPreview,
  RmcpResolvedScope,
  RmcpServer,
  RmcpSession,
  RmcpToolGroup,
} from '../types/rmcp';

// The tool table and the error type live in `rmcpContract.ts` — a leaf module both this file
// and the fixture server import, so the two never form an import cycle. Re-exported here so
// consumers keep a single import site.
export { RMCP_TOOLS, RmcpError } from './rmcpContract';
export type { RmcpErrorKind, RmcpToolName } from './rmcpContract';

/** The single dispatch endpoint. `system` + path are passed separately to `request`, which
 *  composes `/api/terminus` + this. */
const DISPATCH_PATH = '/rmcp/call';

/** The dispatch envelope. A TOOL-level failure is a successful HTTP response carrying
 *  `ok:false` — the same shape MCP itself uses — so a refusal ("not yours") is never conflated
 *  with a transport failure ("the box is down"). Only a genuinely broken request throws. */
interface RmcpEnvelope<T> {
  ok: boolean;
  result?: T;
  error?: { code?: string; message?: string; details?: string[] };
}

/** Server error codes → local kinds. Unknown codes fall to `error`, never to a permissive
 *  reading: an unrecognised refusal must not present as success or as "just retry". */
const ERROR_CODE_KINDS: Record<string, RmcpErrorKind> = {
  forbidden: 'forbidden',
  unauthorized: 'forbidden',
  not_owner: 'forbidden',
  version_conflict: 'conflict',
  conflict: 'conflict',
  not_found: 'not_found',
  upstream_unavailable: 'unavailable',
  unavailable: 'unavailable',
  unknown_tool: 'tool_unavailable',
  tool_not_registered: 'tool_unavailable',
  invalid_argument: 'invalid',
  invalid_pattern: 'invalid',
  invalid_redirect_uri: 'invalid',
};

/** HTTP statuses (thrown by the aggregation client as `HttpStatusError`, whose message carries
 *  the status) → kinds, for the case where dispatch fails before any tool ran. 404/501 is the
 *  "endpoint not deployed yet" shape. */
function kindFromTransport(message: string): RmcpErrorKind {
  if (/\b(404|501)\b/.test(message)) return 'tool_unavailable';
  if (/\b403\b/.test(message)) return 'forbidden';
  if (/\b409\b/.test(message)) return 'conflict';
  if (/\b(502|503|504)\b/.test(message)) return 'unavailable';
  return 'error';
}

/**
 * Call one `rmcp_*` tool. The ONLY outbound call in this module — every typed wrapper below
 * funnels through here, so the transport, the error mapping, and the mock seam each exist once.
 */
async function callTool<T>(tool: RmcpToolName, args: Record<string, unknown> = {}): Promise<T> {
  // THE ONE MOCK BOUNDARY (see rmcpFixtures.ts). `resolveMode()` is the app-wide adapter switch
  // — mock is reached only by an explicit build flag, an explicit server injection, or an
  // explicit per-session opt-in, and unit tests (no `window`) resolve to it. A production
  // bundle is always `http` and therefore always talks to the real tools.
  if (resolveMode() === 'mock') {
    return rmcpFixtureCall<T>(tool, args);
  }

  let envelope: RmcpEnvelope<T>;
  try {
    envelope = await getAggregationClient().request<RmcpEnvelope<T>>('terminus', DISPATCH_PATH, {
      method: 'POST',
      body: JSON.stringify({ tool, args }),
    });
  } catch (e) {
    const message = e instanceof Error ? e.message : 'request failed';
    throw new RmcpError(kindFromTransport(message), tool, message);
  }

  if (!envelope.ok) {
    const code = envelope.error?.code ?? '';
    throw new RmcpError(
      ERROR_CODE_KINDS[code] ?? 'error',
      tool,
      envelope.error?.message ?? `${tool} failed`,
      envelope.error?.details ?? [],
    );
  }
  if (envelope.result === undefined) {
    // `ok:true` with no payload is a contract violation, not an empty result — an empty list is
    // `{result:{clients:[]}}`. Treating it as "nothing here" would render an empty page for what
    // is actually a broken response.
    throw new RmcpError('error', tool, `${tool} returned no result`);
  }
  return envelope.result;
}

// ── Clients ─────────────────────────────────────────────────────────────────

export async function listClients(): Promise<RmcpClient[]> {
  const r = await callTool<{ clients: RmcpClient[] }>(RMCP_TOOLS.clientList);
  return r.clients;
}

export interface CreateClientInput {
  name: string;
  redirectUris: string[];
  /** Mint a client secret (confidential client). Public clients — which is what Claude
   *  registers as — take `false` and authenticate with PKCE alone. */
  confidential: boolean;
  toolGroupIds: string[];
  namespaces: string[];
}

export function createClient(input: CreateClientInput): Promise<RmcpClientCreated> {
  return callTool<RmcpClientCreated>(RMCP_TOOLS.clientCreate, {
    name: input.name,
    redirect_uris: input.redirectUris,
    confidential: input.confidential,
    tool_group_ids: input.toolGroupIds,
    namespaces: input.namespaces,
  });
}

export interface UpdateClientInput {
  id: string;
  /** The version this edit was based on. The server refuses the write if it has moved on. */
  version: number;
  enabled?: boolean;
  redirectUris?: string[];
  toolGroupIds?: string[];
  namespaces?: string[];
}

export async function updateClient(input: UpdateClientInput): Promise<RmcpClient> {
  const r = await callTool<{ client: RmcpClient }>(RMCP_TOOLS.clientUpdate, {
    id: input.id,
    version: input.version,
    enabled: input.enabled,
    redirect_uris: input.redirectUris,
    tool_group_ids: input.toolGroupIds,
    namespaces: input.namespaces,
  });
  return r.client;
}

export function revokeClient(id: string): Promise<void> {
  return callTool<void>(RMCP_TOOLS.clientRevoke, { id }).then(() => undefined);
}

/** The server's own effective-set resolution for this client. See the module doc: this is asked,
 *  never computed. `limit`/`offset` page the returned list so a very large catalog stays bounded
 *  on the wire as well as in the DOM. */
export function resolveClientScope(
  id: string,
  page?: { limit: number; offset: number },
): Promise<RmcpResolvedScope> {
  return callTool<RmcpResolvedScope>(RMCP_TOOLS.clientResolve, {
    id,
    limit: page?.limit,
    offset: page?.offset,
  });
}

// ── Tool groups ─────────────────────────────────────────────────────────────

export async function listGroups(): Promise<RmcpToolGroup[]> {
  const r = await callTool<{ groups: RmcpToolGroup[] }>(RMCP_TOOLS.groupList);
  return r.groups;
}

export async function createGroup(input: {
  name: string;
  description: string;
  patterns: string[];
}): Promise<RmcpToolGroup> {
  const r = await callTool<{ group: RmcpToolGroup }>(RMCP_TOOLS.groupCreate, input);
  return r.group;
}

export async function updateGroup(input: {
  id: string;
  version: number;
  name?: string;
  description?: string;
  patterns?: string[];
}): Promise<RmcpToolGroup> {
  const r = await callTool<{ group: RmcpToolGroup }>(RMCP_TOOLS.groupUpdate, input);
  return r.group;
}

/** Live match preview for a candidate pattern list — the server's matcher against the live
 *  catalog, run BEFORE the group is saved, so an operator sees what a pattern actually selects
 *  rather than guessing. Invalid patterns come back named, because rejection is a write-time
 *  server decision and predicting it here would be a second implementation of the rules. */
export function previewGroup(patterns: string[], limit = 200): Promise<RmcpGroupPreview> {
  return callTool<RmcpGroupPreview>(RMCP_TOOLS.groupPreview, { patterns, limit });
}

// ── Servers / namespaces ────────────────────────────────────────────────────

export async function listServers(): Promise<RmcpServer[]> {
  const r = await callTool<{ servers: RmcpServer[] }>(RMCP_TOOLS.serverOwnerList);
  return r.servers;
}

// ── Sessions / consents ─────────────────────────────────────────────────────

export async function listSessions(clientRowId?: string): Promise<RmcpSession[]> {
  const r = await callTool<{ sessions: RmcpSession[] }>(RMCP_TOOLS.sessionList, {
    client_id: clientRowId,
  });
  return r.sessions;
}

/** Revoke one session, or every session for one client. Exactly one of the two must be given —
 *  an argument-less revoke would be a fleet-wide cut-off by accident, so it is refused here
 *  rather than sent. */
export function revokeSessions(target: { sessionId: string } | { clientRowId: string }): Promise<void> {
  const args =
    'sessionId' in target ? { session_id: target.sessionId } : { client_id: target.clientRowId };
  return callTool<void>(RMCP_TOOLS.sessionRevoke, args).then(() => undefined);
}

/** Human-readable rendering of a failure, for a toast or an inline banner. Kept next to the kind
 *  table so the wording and the classification cannot drift apart. */
export function describeRmcpError(e: unknown): { kind: RmcpErrorKind; message: string } {
  if (e instanceof RmcpError) {
    switch (e.kind) {
      case 'forbidden':
        return { kind: e.kind, message: 'Not permitted for this session — the server refused the change.' };
      case 'conflict':
        return {
          kind: e.kind,
          message: 'Someone else changed this connector while you were editing. Reload to see their version, then re-apply your change.',
        };
      case 'not_found':
        return { kind: e.kind, message: 'This object no longer exists — it may have been revoked elsewhere.' };
      case 'unavailable':
        return { kind: e.kind, message: 'An upstream needed for this call is currently unavailable. Nothing was changed.' };
      case 'tool_unavailable':
        return { kind: e.kind, message: 'Connector administration is not live on this server yet.' };
      case 'invalid':
        return { kind: e.kind, message: e.details.length ? `${e.message}: ${e.details.join('; ')}` : e.message };
      default:
        return { kind: 'error', message: e.message };
    }
  }
  return { kind: 'error', message: e instanceof Error ? e.message : 'Unexpected failure' };
}
