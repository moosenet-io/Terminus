// RMCP-13 (TERM-624): the wire CONTRACT for the connector-administration surface.
//
// These types are a 1:1 transcription of what the `rmcp_*` Terminus tools return — the same
// tools the CLI calls (S132 spec RMCP-06/07/08/11/12). They are DESCRIPTIVE, not authoritative:
// every rule they imply (which tools a client reaches, which patterns are valid, who may edit
// what) is decided on the server. Nothing in this file, and nothing in the panels that consume
// it, re-derives an authorization or a pattern match locally.
//
// Two invariants are worth stating because the natural refactor breaks both:
//
//  1. **Resolution is never computed here.** The "tools this client can currently reach" list
//     comes from `rmcp_client_resolve` and the group match preview comes from
//     `rmcp_group_preview`. A TypeScript matcher that agrees with the server today drifts from
//     it tomorrow, and the whole value of the preview is that it is the SERVER's answer.
//  2. **Absence is denial.** A client with no groups and no namespaces reaches nothing; the UI
//     renders that as an explicit "reaches no tools" state, never as "unknown" or "everything".

/** How a client came to exist (`RegistrationSource` in `src/oauth/model.rs`). A `dcr` client
 *  lands unscoped and disabled for tool access until an operator scopes it (RMCP-08). */
export type RmcpRegistrationSource = 'operator' | 'dcr';

/** One connector, as `rmcp_client_list` / `rmcp_client_create` / `rmcp_client_update` return it.
 *  There is no secret field by design: the secret exists exactly once, in the create response. */
export interface RmcpClient {
  /** Internal row id — the handle every mutating tool takes. */
  id: string;
  /** The public identifier pasted into the client application. */
  clientId: string;
  name: string;
  registrationSource: RmcpRegistrationSource;
  /** Whether the client may reach anything at all. A disabled client is denied at dispatch. */
  enabled: boolean;
  /** Whether the client authenticates at the token endpoint (i.e. it has a secret hash). */
  confidential: boolean;
  redirectUris: string[];
  /** Assigned tool-group ids (see `RmcpToolGroup.id`). Empty ⇒ reaches nothing. */
  toolGroupIds: string[];
  /** Assigned mesh namespaces. Empty ⇒ reaches nothing, regardless of group matches. */
  namespaces: string[];
  /** RFC 3339. */
  createdAt: string;
  /** Optimistic-concurrency token, echoed back on update. A stale value ⇒ `version_conflict`
   *  rather than a silent overwrite of whatever another operator saved in the meantime. */
  version: number;
  /** Whether THIS session may edit this client (server-computed from ownership, RMCP-12).
   *  Cosmetic only — the server refuses the write regardless of what the UI renders. */
  editable: boolean;
}

/** A named set of tool-name patterns (RMCP-06). Empty `patterns` matches nothing. */
export interface RmcpToolGroup {
  id: string;
  name: string;
  description: string;
  /** Exact name, trailing-`*` prefix, or `<namespace>::*`. Validated at write time, server-side. */
  patterns: string[];
  /** Whether this session may edit this group (server-computed, RMCP-12). */
  editable: boolean;
  version: number;
}

/** A mesh namespace as `rmcp_server_owner_list` reports it. `available:false` means the upstream
 *  is currently down — a real, expected state that the UI shows as UNAVAILABLE rather than as an
 *  error, because a scoped-but-down namespace is a config that is fine and an upstream that is not. */
export interface RmcpServer {
  namespace: string;
  /** Display name of the owning account, or null when this session may not see it. */
  ownerName: string | null;
  /** Whether this session owns the namespace and may therefore scope clients to it (RMCP-12). */
  ownedByMe: boolean;
  available: boolean;
  /** Tools currently published by this namespace; null when the upstream is unreachable. */
  toolCount: number | null;
}

/** One concrete tool in a resolved preview, as the SERVER resolved it. */
export interface RmcpResolvedTool {
  /** Fully-qualified (namespaced) tool name. */
  name: string;
  namespace: string;
  /** Which assigned group matched it — the "why is this here?" answer an operator needs. */
  matchedGroup: string;
  /** The pattern inside that group that matched. */
  matchedPattern: string;
  /** False when the tool's namespace is currently unreachable: it is in scope but not callable
   *  right now. Distinct from being out of scope, and shown distinctly. */
  available: boolean;
}

/** `rmcp_client_resolve` — the effective set for one client against the live catalog. This is the
 *  single most load-bearing read on the page: it is the server's own `effective(...)` answer
 *  (RMCP-07), not a client-side re-derivation of it. */
export interface RmcpResolvedScope {
  clientId: string;
  tools: RmcpResolvedTool[];
  /** Namespaces assigned to the client that are currently unreachable. */
  unavailableNamespaces: string[];
  /** Set when the server capped the returned list; `tools.length` is then a prefix, and the UI
   *  says so instead of implying the client reaches only that many. */
  truncated: boolean;
  /** Catalog generation the resolution was computed against — shown so a stale preview is
   *  recognisable as stale. */
  catalogGeneration: string;
}

/** `rmcp_group_preview` — the server's match of a candidate pattern list against the live
 *  catalog, used for the group editor's live preview BEFORE the group is saved. */
export interface RmcpGroupPreview {
  /** Concrete matching tool names, server-resolved. */
  tools: RmcpResolvedTool[];
  /** Patterns the server refused, with the reason. Rejection is a write-time server decision
   *  (RMCP-06); the editor surfaces it, it does not predict it. */
  invalidPatterns: { pattern: string; reason: string }[];
  truncated: boolean;
}

/** One live grant — a consent plus the token families hanging off it (RMCP-11). */
export interface RmcpSession {
  id: string;
  /** Account display name. A delegated owner only ever sees their own objects (server-scoped). */
  accountName: string;
  clientRowId: string;
  clientName: string;
  scope: string;
  grantedAt: string;
  /** RFC 3339, or null if never used since issuance. */
  lastUsedAt: string | null;
  /** Live (non-revoked, non-expired) refresh-token families under this consent. */
  activeFamilies: number;
  revokedAt: string | null;
}

/** Create-client result. `clientSecret` is present exactly once, here, and is never returned by
 *  any read tool — the store holds only an argon2id hash (RMCP-08). */
export interface RmcpClientCreated {
  client: RmcpClient;
  /** Null for a public client (no secret was minted). */
  clientSecret: string | null;
}

// ── Accounts (TERM #654) ──────────────────────────────────────────────────────────────────────
//
// The human identity the OAuth door authenticates: an account logs in at `/oauth/login`, grants
// consent, and is named as a connector's `owner`. Distinct from a fleet `Principal` — an account
// MAPS to one (RMCP-05), it does not replace one.
//
// **There is no password field, in either direction.** It is not returned by any read (the server
// stores an argon2id hash), it is not held in any state that outlives the create call, and it is
// never round-tripped into a form. The only place a password appears in this app is the create
// dialog's input, for the life of that submit.

/** One account, as `rmcp_account_list` returns it. */
export interface RmcpAccount {
  id: string;
  /** What the person types at `/oauth/login`. */
  account: string;
  /** Holds fleet-operator authority. Server-computed and server-enforced; the UI only shows it. */
  operator: boolean;
  /** A disabled account cannot log in, consent, or satisfy any authorization it held. */
  disabled: boolean;
  /** RFC 3339. */
  createdAt: string;
}

/**
 * The whole account view, including the two states in which there is nothing to list.
 *
 * `bootstrapAvailable` and `stranded` are the SERVER's answer, not inferred from
 * `accounts.length` — they are different facts and conflating them is what sends an operator to
 * run a command that cannot work:
 *
 *  - `bootstrapAvailable` — this door has never had an account, so the first-operator path is
 *    open. The only state in which anyone may create an account without being an operator.
 *  - `stranded` — accounts exist but none is an active operator. Nothing can administer the door
 *    and the first-account path will NOT reopen (it is gated on account existence, not on
 *    operator existence). Needs direct database access to fix.
 *
 * Both false with an empty `accounts` is not a state the server produces; if it ever appears,
 * render it as "no accounts", never as "everything is fine".
 */
export interface RmcpAccountsView {
  accounts: RmcpAccount[];
  bootstrapAvailable: boolean;
  stranded: boolean;
}

/** What `rmcp_account_create` returns. No secret: the password was the caller's own input and is
 *  never echoed — unlike a client secret, which the SERVER mints and therefore must show once. */
export interface RmcpAccountCreated {
  id: string;
  account: string;
  operator: boolean;
  /** Whether this call was the one-shot first-account creation. */
  bootstrap: boolean;
}
