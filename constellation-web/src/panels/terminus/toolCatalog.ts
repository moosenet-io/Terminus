// CGUI-05 (TERM #528): pure, deterministic derivation of per-tool DEPTH for the Terminus
// Tools catalog. The audit found the Tools panel showed the tool NAME only — this module is
// the seam that turns a bare `{module}_{action}` name into the full per-tool detail the guide
// asks for (category, description, schema/params, rate-limit, auth identity, enable state, and
// last-invocation telemetry).
//
// DATA PROVENANCE (the item requires calling out real vs placeholder — same discipline as
// CGUI-04's ModuleDetail):
//   • name, module          — REAL (aggregation client `terminus.configSummary()` → modules[].tools).
//   • enabled               — REAL (the owning module's `enabled` flag).
//   • category, description  — DERIVED deterministically from the tool NAME (verb/noun parse).
//                              Representative — no machine-readable category/summary is exposed
//                              by the client yet (pending the CGUI-08 data client).
//   • params (schema)        — DERIVED representative placeholder from the name; the real MCP
//                              JSON-Schema per tool is not exposed by the client yet (CGUI-08).
//   • rateLimit, auth        — Representative per-module placeholder (CGUI-08 real-data wiring).
//   • lastInvocation         — Synthetic, deterministic telemetry stand-in (no per-tool
//                              invocation stream yet — CGUI-08). Some tools read as never-invoked.
//
// Everything here is PURE + deterministic so the panel reads coherently and a unit test can pin
// the shapes. Swap the derivations for CGUI-08 client reads once that data exists.

export type ToolCategory = 'read' | 'write' | 'admin' | 'search';

export interface ToolParam {
  name: string;
  /** Representative JSON-Schema-ish type (string/number/boolean/object). */
  type: string;
  required: boolean;
  desc: string;
}

export interface ToolLastInvocation {
  /** RFC 3339 UTC timestamp of the stand-in "last call". */
  ts: string;
  result: 'ok' | 'error';
  /** Pre-humanised "3m ago" relative label (computed against `now`). */
  ago: string;
  /** Synthetic call latency in ms (deterministic in the tool name) — the POL-06 Latency column. */
  latencyMs: number;
}

export interface ToolDetail {
  /** REAL — the registered `{module}_{action}` name. */
  name: string;
  /** REAL — owning module (the tool-name prefix / config group). */
  module: string;
  /** REAL — inherited from the owning module's enabled flag. */
  enabled: boolean;
  /** DERIVED — read/write/admin/search bucket from the action verb. */
  category: ToolCategory;
  /** DERIVED — one-line human summary from the name. */
  description: string;
  /** DERIVED representative — the tool's argument schema. */
  params: ToolParam[];
  /** Representative placeholder — per-module rate limit. */
  rateLimit: string;
  /** Representative placeholder — the vault identity the call authenticates with. */
  auth: string;
  /** Synthetic placeholder — last invocation, or null for a never-invoked tool. */
  lastInvocation: ToolLastInvocation | null;
}

// ── verb → category / prose ──────────────────────────────────────────────────────────────────
const READ_VERBS = new Set(['list', 'get', 'export', 'read', 'fetch']);
const SEARCH_VERBS = new Set(['search']);
const ADMIN_VERBS = new Set(['delete', 'remove', 'merge']);
// everything else (create/update/add/assign/bulk/…) is a write.

function categoryFor(verb: string): ToolCategory {
  if (SEARCH_VERBS.has(verb)) return 'search';
  if (READ_VERBS.has(verb)) return 'read';
  if (ADMIN_VERBS.has(verb)) return 'admin';
  return 'write';
}

/** Split a `{module}_{action}` name into its module prefix and remaining action tokens. Robust
 *  to a name with no underscore (whole name is the action, module is the passed-in owner). */
function splitAction(name: string, module: string): { verb: string; nounWords: string[] } {
  const rest = name.startsWith(`${module}_`) ? name.slice(module.length + 1) : name;
  const tokens = rest.split('_').filter(Boolean);
  const verb = tokens[0] ?? rest;
  return { verb, nounWords: tokens.slice(1) };
}

function humanize(verb: string, nounWords: string[]): string {
  const noun = nounWords.join(' ');
  const cap = verb.charAt(0).toUpperCase() + verb.slice(1);
  return noun ? `${cap} ${noun}` : cap;
}

// The set of entity nouns that carry an id-shaped argument (drives the derived schema).
const ENTITY_WORDS = new Set([
  'item', 'items', 'issue', 'issues', 'pr', 'repo', 'repos', 'project', 'comment', 'comments',
  'state', 'states', 'cycle', 'cycles', 'module', 'modules', 'label', 'labels', 'member',
  'members', 'attachment', 'attachments', 'file', 'work',
]);

/** Singular-ish entity token from the noun words, or a generic 'resource'. */
function entityOf(nounWords: string[]): string {
  const hit = nounWords.find(w => ENTITY_WORDS.has(w));
  const base = hit ?? nounWords[nounWords.length - 1] ?? 'resource';
  return base.replace(/s$/, '');
}

/** Representative argument schema derived from the verb + noun. Always returns at least one row
 *  so the schema region is never empty — placeholder pending real MCP schema (CGUI-08). */
export function paramsFor(verb: string, nounWords: string[]): ToolParam[] {
  const entity = entityOf(nounWords);
  const params: ToolParam[] = [];

  if (verb === 'search') {
    params.push({ name: 'query', type: 'string', required: true, desc: `Full-text ${entity} search query` });
    params.push({ name: 'limit', type: 'number', required: false, desc: 'Max results (default 25)' });
    return params;
  }
  if (verb === 'list') {
    params.push({ name: 'limit', type: 'number', required: false, desc: 'Page size (default 25)' });
    params.push({ name: 'cursor', type: 'string', required: false, desc: 'Opaque pagination cursor' });
    return params;
  }
  if (verb === 'get' || verb === 'export') {
    params.push({ name: `${entity}_id`, type: 'string', required: true, desc: `Target ${entity} identifier` });
    return params;
  }
  if (verb === 'delete' || verb === 'remove') {
    params.push({ name: `${entity}_id`, type: 'string', required: true, desc: `${entity} to remove` });
    return params;
  }
  if (verb === 'create') {
    params.push({ name: 'fields', type: 'object', required: true, desc: `New ${entity} attributes` });
    return params;
  }
  if (verb === 'update') {
    params.push({ name: `${entity}_id`, type: 'string', required: true, desc: `${entity} to update` });
    params.push({ name: 'fields', type: 'object', required: true, desc: 'Partial attributes to patch' });
    return params;
  }
  if (verb === 'assign' || verb === 'add') {
    params.push({ name: `${entity}_id`, type: 'string', required: true, desc: `${entity} to attach` });
    params.push({ name: 'target_id', type: 'string', required: true, desc: 'Parent to attach it to' });
    return params;
  }
  if (verb === 'merge') {
    params.push({ name: `${entity}_id`, type: 'string', required: true, desc: `${entity} to merge` });
    params.push({ name: 'strategy', type: 'string', required: false, desc: 'merge | squash | rebase' });
    return params;
  }
  // Fallback: a single representative id argument.
  params.push({ name: `${entity}_id`, type: 'string', required: false, desc: `Optional ${entity} scope` });
  return params;
}

// ── per-module representative config (placeholder — CGUI-08 real-data wiring) ──────────────────
// The vault identity each module's tools authenticate with (real credential NAMES from the
// fleet's secrets discipline; the values live in <secret-manager>, never here). Rate limits are
// representative per-module defaults.
interface ModulePolicy { auth: string; rate: string; }
const MODULE_POLICY: Record<string, ModulePolicy> = {
  plane:   { auth: 'PLANE_API_KEY',  rate: '20 / min' },
  gitea:   { auth: 'GITEA_TOKEN',    rate: '60 / min' },
  github:  { auth: 'GITHUB_TOKEN',   rate: '30 / min' },
  nexus:   { auth: 'NEXUS_TOKEN',    rate: '60 / min' },
  commute: { auth: 'COMMUTE_TOKEN',  rate: '60 / min' },
};
const DEFAULT_POLICY: ModulePolicy = { auth: 'vault-managed', rate: '60 / min' };

// ── deterministic synthetic telemetry (stand-in, CGUI-08) ──────────────────────────────────────
// Fixed base epoch for all synthetic "last invocation" stamps. The whole GUI must render
// deterministically offline (same fixture input → identical output across calls, so snapshot
// tests and offline renders are stable) — so the telemetry NEVER reads the wall clock. This is
// a placeholder anchor; when the CGUI-08 real invocation stream lands it supplies real stamps.
export const FIXTURE_NOW = Date.parse('2026-07-25T04:00:00Z');

/** FNV-1a-ish hash so per-tool telemetry is stable across renders (no flicker) yet varied. */
function hashName(name: string): number {
  let h = 2166136261;
  for (let i = 0; i < name.length; i++) {
    h ^= name.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

function relativeAgo(ms: number): string {
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

/**
 * Synthetic last-invocation for a tool. Deterministic in the tool name so it never flickers.
 * ~1 in 6 tools read as never-invoked (null); ~1 in 8 of the rest as an error. `now` injectable
 * for testing. NOT live data — placeholder until the CGUI-08 invocation stream exists.
 */
export function lastInvocationFor(name: string, now: number = FIXTURE_NOW): ToolLastInvocation | null {
  const h = hashName(name);
  if (h % 6 === 0) return null; // never invoked
  const ageMs = (h % 5400) * 1000; // 0..90 min back, deterministic
  const result: 'ok' | 'error' = h % 8 === 3 ? 'error' : 'ok';
  // Deterministic latency stand-in: ~20–1020ms, varied by a different slice of the hash so it
  // doesn't track the age. Errors read slower (a timeout-ish tail) for a truthful feel.
  const base = 20 + ((h >>> 7) % 1000);
  const latencyMs = result === 'error' ? base + 800 : base;
  return { ts: new Date(now - ageMs).toISOString(), result, ago: relativeAgo(ageMs), latencyMs };
}

/** Humanise a latency in ms for the Latency column (mono). */
export function fmtLatency(ms: number): string {
  return ms >= 1000 ? `${(ms / 1000).toFixed(2)}s` : `${ms}ms`;
}

/**
 * Build the full per-tool detail from the one real fact we have (name), its owning module, and
 * the module's enabled flag. `now` injectable so telemetry stamps are testable.
 */
export function deriveToolDetail(
  module: string,
  name: string,
  enabled: boolean,
  now: number = FIXTURE_NOW,
): ToolDetail {
  const { verb, nounWords } = splitAction(name, module);
  const policy = MODULE_POLICY[module] ?? DEFAULT_POLICY;
  return {
    name,
    module,
    enabled,
    category: categoryFor(verb),
    description: humanize(verb, nounWords),
    params: paramsFor(verb, nounWords),
    rateLimit: policy.rate,
    auth: policy.auth,
    lastInvocation: enabled ? lastInvocationFor(name, now) : null,
  };
}

// Category → Badge tone (inside the Badge tone union): read=blue, write=green, search=amber,
// admin=rose.
export const CATEGORY_BADGE: Record<ToolCategory, 'blue' | 'green' | 'amber' | 'rose'> = {
  read: 'blue',
  write: 'green',
  search: 'amber',
  admin: 'rose',
};

// S127 TGUI2 M6: the kind chip is now a NEUTRAL outline chip carrying only a tiny leading dot in
// its semantic color (read/write/search/admin scannability) — the chip body stays neutral so a
// data-dense row has at most one saturated token (its status pill). These are the dot inks.
export const CATEGORY_DOT_COLOR: Record<ToolCategory, string> = {
  read: 'var(--flux-blue-soft)',
  write: 'var(--flux-green-soft)',
  search: 'var(--flux-amber)',
  admin: 'var(--flux-rose-soft)',
};
