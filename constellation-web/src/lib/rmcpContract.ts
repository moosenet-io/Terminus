// RMCP-13 (TERM-624): the `rmcp_*` tool contract shared by the API client and the fixture
// server. A LEAF module on purpose — `rmcpClient.ts` calls the fixture server and the fixture
// server raises the same error type the client does, so the tool table and the error class have
// to live somewhere neither imports the other to reach. Everything here is contract, no I/O.

/** The tool names this page calls. Kept as one frozen table rather than string literals at each
 *  call site so the full set of tools the GUI depends on is greppable in one place — and so a
 *  renamed tool is one edit, not a hunt. */
export const RMCP_TOOLS = {
  clientList: 'rmcp_client_list',
  clientCreate: 'rmcp_client_create',
  clientUpdate: 'rmcp_client_update',
  clientRevoke: 'rmcp_client_revoke',
  clientResolve: 'rmcp_client_resolve',
  groupList: 'rmcp_group_list',
  groupCreate: 'rmcp_group_create',
  groupUpdate: 'rmcp_group_update',
  groupPreview: 'rmcp_group_preview',
  serverOwnerList: 'rmcp_server_owner_list',
  // TERM #654 — the account surface. Every other tool in this table presupposes an account;
  // until these existed, `rmcp_account` was empty and unpopulatable and the whole door reached
  // nothing. `accountCreate` is also the BOOTSTRAP path (see `types/rmcp.ts`).
  accountList: 'rmcp_account_list',
  accountCreate: 'rmcp_account_create',
  accountPromote: 'rmcp_account_promote',
  accountDisable: 'rmcp_account_disable',
  sessionList: 'rmcp_session_list',
  sessionRevoke: 'rmcp_session_revoke',
} as const;

export type RmcpToolName = (typeof RMCP_TOOLS)[keyof typeof RMCP_TOOLS];

// ── Selector rule (contract, not implementation) ──────────────────────────────────────────────
//
// **Selectors are MUTUALLY EXCLUSIVE, and exactly one must be given.** A request naming none, or
// more than one, is refused with `invalid_argument`. It is never resolved by precedence, and
// never treated as "no rows matched".
//
// Stated here, once, as a property of the API rather than as an accident of some handler's `if`
// order — which is exactly how the two failures below slipped in independently. Both are the same
// bug wearing different hats, and both are worst on a destructive control reached for mid-incident:
//
//   • NO selector (round 2)   — revoked nothing, reported success. The operator believes access
//                               was cut and stops investigating.
//   • BOTH selectors (round 4) — asked for two things, got whichever the implementation happened
//                               to test first, and was told it succeeded. "I did part of what you
//                               asked and called it done" is worse than a refusal, because the
//                               part that was skipped is invisible.
//
// Today this binds `rmcp_session_revoke` (`session_id` XOR `client_id`) — the only tool here that
// accepts more than one selector. It is written as a general rule so the next multi-selector tool
// inherits it instead of re-deriving it, and so the eventual server implements the same one: the
// TypeScript wrapper's argument union is a compile-time courtesy, not a control, and the server
// is the boundary that has to hold against callers that never saw it.
export const RMCP_SELECTOR_RULE =
  'exactly one selector must be given; none and more-than-one are both invalid_argument';

/** Machine-readable failure kinds. Each maps to a distinct thing the operator should DO, which
 *  is the only reason to distinguish them:
 *   - `forbidden`      — the server refused: not yours to touch (RMCP-12). Not a bug.
 *   - `conflict`       — someone else saved first; reload and re-apply. Never overwrite.
 *   - `not_found`      — the object went away underneath this view.
 *   - `unavailable`    — an upstream this call needed is down. Transient, not a misconfiguration.
 *   - `tool_unavailable` — the `rmcp_*` tool isn't deployed on this server yet.
 *   - `invalid`        — the server rejected the input (e.g. a redirect URI or a pattern).
 *   - `error`          — anything else, including transport failure.
 */
export type RmcpErrorKind =
  | 'forbidden'
  | 'conflict'
  | 'not_found'
  | 'unavailable'
  | 'tool_unavailable'
  | 'invalid'
  | 'error';

export class RmcpError extends Error {
  readonly kind: RmcpErrorKind;
  readonly tool: string;
  /** Field-level detail when the server rejected specific inputs (e.g. bad redirect URIs). */
  readonly details: string[];

  constructor(kind: RmcpErrorKind, tool: string, message: string, details: string[] = []) {
    super(message);
    this.name = 'RmcpError';
    this.kind = kind;
    this.tool = tool;
    this.details = details;
  }
}
