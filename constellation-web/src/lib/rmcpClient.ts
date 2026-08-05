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
//
// THE FIXTURE SERVER IS NOT IN THE PRODUCTION BUNDLE, STRUCTURALLY.
// ------------------------------------------------------------------------------------------
// An earlier revision imported `rmcpFixtures.ts` at the top level and let `resolveMode()` decide
// at RUNTIME whether it ran. Review round 1 rejected that, correctly: "a production build always
// resolves to http" was a claim in a comment, and the failure it guards against is a UI showing
// FABRICATED authorization data to an operator making real scoping decisions. That claim has to
// be structural.
//
// So the fixture is now reached only through a dynamic `import()` behind a literal
// `!import.meta.env.PROD` guard. Vite replaces that text with `false` at transform time, the
// branch folds away, and the module is never referenced from a production build's graph — no
// chunk is emitted for it. `scripts/assert-http-bundle.mjs` (the last step of `npm run build`,
// so there is no unguarded build path) asserts the fixture's marker string is absent from every
// shipped asset, which fails the build if that ever stops being true.
//
// The consequence is worth stating: in a production bundle, a runtime `?mock` opt-in gives the
// REST of the app fixtures but gives this page nothing — its calls go to the real endpoint and
// report `tool_unavailable` if it is not there. That asymmetry is deliberate. Connector scoping
// is the one surface where plausible-looking fake data is worse than an empty page.
import { getAggregationClient, resolveMode } from './aggregationClient';
import { RMCP_TOOLS, RmcpError } from './rmcpContract';
import type { RmcpErrorKind, RmcpToolName } from './rmcpContract';
import type {
  RmcpAccount,
  RmcpAccountCreated,
  RmcpAccountsView,
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

/** The tools that carry a `version` and can therefore lose an optimistic-concurrency race.
 *  Only these get the "reload and re-apply" wording for a `conflict`; see `describeRmcpError`.
 *  Derived from the tools that actually take a version argument, not from a name prefix, so a
 *  new tool joins this set by acquiring the property rather than by being spelled a certain way. */
const OPTIMISTIC_CONCURRENCY_TOOLS: ReadonlySet<string> = new Set<string>([
  RMCP_TOOLS.clientUpdate,
  RMCP_TOOLS.groupUpdate,
]);

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
  // THE ONE MOCK BOUNDARY (see rmcpFixtures.ts and the module doc above). Two conditions, in
  // this order, and the order is the point: `import.meta.env.PROD` is a literal Vite replaces at
  // BUILD time, so in a production build this whole branch — including the `import()` and
  // therefore the fixture module itself — is folded away before the bundle exists. The runtime
  // `resolveMode()` check only ever narrows further, inside dev/test builds.
  if (!import.meta.env.PROD && resolveMode() === 'mock') {
    const { rmcpFixtureCall } = await import('./rmcpFixtures');
    return rmcpFixtureCall<T>(tool, args);
  }

  let envelope: RmcpEnvelope<T>;
  try {
    envelope = await getAggregationClient().request<RmcpEnvelope<T>>(
      'terminus',
      DISPATCH_PATH,
      { method: 'POST', body: JSON.stringify({ tool, args }) },
      // A TOOL refusal is an HTTP 200 carrying `ok:false`, so without this
      // classifier the activity/toast layer saw a settled request and announced
      // SUCCESS — while the panel showed the refusal inline. Review round 3
      // (codex) found it on the account mutations; it was never account-specific
      // and applied to every connector write too, which is why the fix is here
      // at the single dispatch rather than at a call site. This is exactly the
      // `isOk` seam MACT-03 added for "a mutating call that degrades instead of
      // throwing", finally used by the caller that most needed it.
      envelope => envelope?.ok === true,
    );
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
  /** The account the connector will belong to. REQUIRED, with no default and no inference —
   *  see the note on `createClient`. */
  owner: string;
  /** The account performing the creation. REQUIRED for the same reason `owner` is. */
  actor: string;
  name: string;
  redirectUris: string[];
  /** Mint a client secret (confidential client). Public clients — which is what Claude
   *  registers as — take `false` and authenticate with PKCE alone. */
  confidential: boolean;
  toolGroupIds: string[];
  namespaces: string[];
}

/**
 * Mint a connector. TERM-647.
 *
 * **`owner` and `actor` are carried, never invented.** `rmcp_client_create` requires both and
 * refuses to default either (RMCP-08). The reason is worth restating at the call site, because
 * the tempting "fix" for a missing field is to fill it in: these tools reach Terminus over the
 * fleet's own transports, which authenticate a MESH PRINCIPAL rather than an `rmcp_account`, so
 * there is no authenticated OAuth identity here to read an owner from. A layer that picked one
 * anyway would be making an authorization decision by guessing, silently.
 *
 * That reasoning binds this module exactly as it binds the tool. This function must therefore
 * never derive either value — not from the session, not from `listServers()`, and not by
 * copying one into the other. Both arrive from a human choice made in the dialog above and are
 * passed through unchanged; an unknown or disabled account comes back `not_found` and an
 * unauthorized pairing comes back `forbidden`, both from the server.
 */
export function createClient(input: CreateClientInput): Promise<RmcpClientCreated> {
  return callTool<RmcpClientCreated>(RMCP_TOOLS.clientCreate, {
    actor: input.actor,
    owner: input.owner,
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

// ── Accounts (TERM #654) ────────────────────────────────────────────────────
//
// The bootstrap surface. Read the module doc above first — everything there about ONE path to
// the backend and AUTHORIZATION IS NOT HERE applies unchanged, and applies hardest here: this is
// the only tool family with a path that runs before any account exists, so a client-side guess
// about who may call it would be a guess about the one call nobody is authenticated for.
//
// **THE PASSWORD GOES ONE WAY.** It is a parameter of `createAccount` and appears nowhere else
// in this module: no read returns it, nothing stores it, and no wrapper accepts it back. That is
// deliberately unlike `createClient`, whose response carries a client secret the SERVER minted
// and the UI must show exactly once. A password is the operator's own input; echoing it would add
// a disclosure with no purpose.
//
// **`actor` is passed through, never invented** — the same rule, and the same reasoning, as
// `createClient`'s `owner`/`actor` (see its doc). It is optional here because the server can
// resolve a sole operator unambiguously, and when it cannot it REFUSES rather than picking one.
// This layer must never fill it in to make a refusal go away.

/** The account view, including the two no-account states. Absence is reported by the server as
 *  an empty list plus the flags — never inferred here from a failed call. */
export async function listAccounts(actor?: string): Promise<RmcpAccountsView> {
  const r = await callTool<{
    accounts: RmcpAccountWire[];
    bootstrap_available: boolean;
    stranded: boolean;
  }>(RMCP_TOOLS.accountList, { actor });
  return {
    // Mapped FIELD BY FIELD, not spread. Review round 2 (codex) caught the
    // spread version: the tool emits `created_at` and `RmcpAccount` declares
    // `createdAt`, so every row arrived with `createdAt === undefined` and the
    // page's `a.createdAt.slice(0, 10)` threw for every non-empty result — a
    // page that worked only while the list was empty. TypeScript could not see
    // it because the wire type was asserted rather than described; `RmcpAccountWire`
    // is now the honest shape, so the compiler checks the translation.
    accounts: (r.accounts ?? []).map(row => wireAccount(row, RMCP_TOOLS.accountList)),
    // `?? false` is the fail-closed reading for BOTH: a response that omits
    // `bootstrap_available` must not offer the bootstrap, and one that omits
    // `stranded` must not claim the door is healthy — the page renders the
    // ordinary "no accounts" copy in that case, which is true either way.
    bootstrapAvailable: r.bootstrap_available ?? false,
    stranded: r.stranded ?? false,
  };
}

/** The account rows exactly as the tool emits them: snake_case, unmapped. Declared so the
 *  translation below is type-checked rather than assumed. */
interface RmcpAccountWire {
  id: string;
  account: string;
  operator: boolean;
  disabled: boolean;
  created_at: string;
}

/**
 * Translate one wire row, REFUSING a malformed one.
 *
 * Round 5 (codex) found the previous version's `a.disabled === true` doing exactly what its own
 * comment said it was avoiding. A missing or malformed `disabled` became `false` — "enabled" —
 * and if `operator` was validly `true` the page then counted that row as an ACTIVE OPERATOR. That
 * feeds `actorIsAmbiguous` and `wouldStrandTheDoor`, so a malformed row could make the GUI believe
 * there is a spare operator and offer to demote the real one. The reassuring direction, on
 * authority, from absent data.
 *
 * The mapper cannot resolve that safely by guessing either way: "assume disabled" hides a real
 * account, "assume enabled" invents an operator. So it does neither and REFUSES — an operator
 * seeing an error and reaching for the CLI is strictly better than one acting on a listing that
 * quietly misrepresents who can administer the door. Absence is not the empty set here; absence
 * is a broken response, and this module's contract already says an `ok:true` with a malformed
 * payload is a contract violation rather than data.
 */
function wireAccount(a: RmcpAccountWire, tool: RmcpToolName): RmcpAccount {
  const bad = (field: string): never => {
    throw new RmcpError(
      'error',
      tool,
      `the server returned an account row with a missing or malformed ${field}`,
    );
  };
  if (typeof a?.id !== 'string' || !a.id) bad('id');
  if (typeof a?.account !== 'string' || !a.account) bad('account');
  if (typeof a?.operator !== 'boolean') bad('operator');
  if (typeof a?.disabled !== 'boolean') bad('disabled');
  if (typeof a?.created_at !== 'string' || !a.created_at) bad('created_at');
  return {
    id: a.id,
    account: a.account,
    operator: a.operator,
    disabled: a.disabled,
    createdAt: a.created_at,
  };
}

export interface CreateAccountInput {
  /** The operator performing this. Optional — see the note above. NEVER defaulted locally. */
  actor?: string;
  account: string;
  /** Sent once, held nowhere. */
  password: string;
  operator: boolean;
}

export function createAccount(input: CreateAccountInput): Promise<RmcpAccountCreated> {
  return callTool<RmcpAccountCreated>(RMCP_TOOLS.accountCreate, {
    actor: input.actor,
    name: input.account,
    password: input.password,
    operator: input.operator,
  });
}

/** Grant or withdraw operator authority. The server refuses to remove the last active operator;
 *  this wrapper does not pre-empt that check, it surfaces the refusal. */
export function setAccountOperator(account: string, operator: boolean, actor?: string): Promise<void> {
  return callTool<unknown>(RMCP_TOOLS.accountPromote, {
    actor,
    account,
    revoke: !operator,
  }).then(() => undefined);
}

/** Disable or re-enable. Same last-operator refusal, same posture toward it. */
export function setAccountDisabled(account: string, disabled: boolean, actor?: string): Promise<void> {
  return callTool<unknown>(RMCP_TOOLS.accountDisable, {
    actor,
    account,
    enable: !disabled,
  }).then(() => undefined);
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

/** Revoke one session, or every session for one client — EXACTLY one (`RMCP_SELECTOR_RULE`).
 *
 *  The union expresses that at compile time; the check below re-states it at RUNTIME, because
 *  types are erased and this is reachable from untyped JS. A union member is also not an
 *  exclusive-or structurally: an object carrying BOTH fields satisfies `{sessionId: string}`, so
 *  even a typed caller can pass one.
 *
 *  Neither ambiguous nor empty is resolved here. An earlier version took `'sessionId' in target`
 *  first, which silently prioritised it — the operator asked for two revocations, got one, and was
 *  told it succeeded, with no way to see which. On a control reached for mid-incident that is
 *  worse than a refusal. The server enforces the same rule independently. */
export function revokeSessions(target: { sessionId: string } | { clientRowId: string }): Promise<void> {
  const { sessionId, clientRowId } = target as { sessionId?: string; clientRowId?: string };
  if (sessionId && clientRowId) {
    return Promise.reject(
      new RmcpError(
        'invalid',
        RMCP_TOOLS.sessionRevoke,
        'a revoke must name either a session or a client, not both',
      ),
    );
  }
  if (!sessionId && !clientRowId) {
    return Promise.reject(
      new RmcpError('invalid', RMCP_TOOLS.sessionRevoke, 'a revoke must name a session or a client'),
    );
  }
  const args = sessionId ? { session_id: sessionId } : { client_id: clientRowId };
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
        // A `conflict` means two different things, and the right words differ completely.
        //
        // For the VERSION-CARRYING tools it is optimistic concurrency — somebody saved first —
        // and the useful answer is procedural: reload, re-apply. The server's own message
        // ("stale") is useless to a human, so the copy is written here.
        //
        // For everything else it is a REFUSAL whose whole value is the sentence the server
        // wrote: "that is this deployment's last active operator", "this fleet has more than
        // one operator account; name the acting one", "an account with that name already
        // exists". Round 3 (codex) caught the account tools being given the connector copy —
        // which told the operator to reload and retry a write that can never succeed, and hid
        // the actual reason completely.
        return {
          kind: e.kind,
          message: OPTIMISTIC_CONCURRENCY_TOOLS.has(e.tool)
            ? 'Someone else changed this connector while you were editing. Reload to see their version, then re-apply your change.'
            : e.message,
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
