#!/usr/bin/env node
// S127 TGUI2 (DATA-01): post-build guard — refuse to ship a bundle whose RESOLVED DEFAULT
// adapter is `mock`, so the "smoke-and-mirrors" fixtures can never be deployed as the embedded
// dist. Runs as the last step of `npm run build` (see package.json), so EVERY build is guarded —
// there is no unguarded build path.
//
// The decision does NOT grep the minified bundle for a quote-sensitive string (the previous
// version did, which was both too strict — single-quoted `'http'` output would false-fail — and
// too weak — a mock build that merely CONTAINED the string `"http"` would false-pass). Instead it
// evaluates the resolved default from the BUILD-TIME signal using the same precedence as
// src/lib/aggregationClient.ts `resolveMode()`:
//
//   VITE_AGG_MODE === 'http'  -> http   (explicit http build)
//   VITE_AGG_MODE === 'mock'  -> mock   -> FAIL (a mock build must never be the shipped dist)
//   VITE_AGG_MODE unset/other -> http   (the production runtime is a browser served same-origin
//                                         by the real terminus binary, so resolveMode() returns
//                                         'http' there — this is the whole point of the S127
//                                         inverted default)
//
// Because the pass/fail decision comes from the build env, it is completely independent of the
// minifier's quote style.
//
// A secondary sanity check confirms the runtime-selection SEAMS survived minification — but ONLY
// for the runtime-default build (VITE_AGG_MODE unset), which is what the committed/deployed dist
// uses and what relies on resolveMode()'s runtime branches. When VITE_AGG_MODE=http is set
// explicitly, Vite statically inlines the early `return 'http'` and legitimately dead-code-
// eliminates those runtime branches — the bundle is still correctly http, so we do NOT require
// the seams there (requiring them would be a false failure — the same over-strictness the rewrite
// set out to remove).
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

function fail(msg) {
  console.error(`[assert-http-bundle] FAIL: ${msg}`);
  process.exit(1);
}

// ── 1. Resolved-default decision (quote-style-independent, from the build env) ──────────────
const buildMode = process.env.VITE_AGG_MODE;
/** Faithful replay of resolveMode()'s build-time precedence; the production runtime (a browser)
 *  resolves an unset/other value to 'http'. */
function resolvedDefault(mode) {
  if (mode === 'http') return 'http';
  if (mode === 'mock') return 'mock';
  return 'http';
}
const resolved = resolvedDefault(buildMode);
console.log(`[assert-http-bundle] VITE_AGG_MODE=${buildMode ?? '(unset)'} -> resolved default: ${resolved}`);
if (resolved !== 'http') {
  fail(`resolved default adapter is '${resolved}', not 'http' — refusing to ship a mock-defaulting bundle. `
    + `Build without VITE_AGG_MODE (or with VITE_AGG_MODE=http) for the embedded/deployed dist.`);
}

// ── 2. Secondary sanity (runtime-default build only): the runtime seams survived minification ─
// Only meaningful when the default is resolved AT RUNTIME (VITE_AGG_MODE unset) — the committed/
// deployed dist. For an explicit VITE_AGG_MODE=http build the seams are correctly DCE'd, so skip.
if (buildMode === undefined) {
  const ROOT = join(fileURLToPath(new URL('.', import.meta.url)), '..');
  const ASSETS = join(ROOT, 'dist', 'assets');
  let bundle;
  try {
    const name = readdirSync(ASSETS).find(f => /^index-.*\.js$/.test(f));
    if (!name) throw new Error('no index-*.js bundle found in dist/assets');
    bundle = readFileSync(join(ASSETS, name), 'utf8');
    console.log(`[assert-http-bundle] inspecting dist/assets/${name}`);
  } catch (e) {
    fail(e.message);
  }
  // String/identifier tokens preserved verbatim by the minifier (quote-style-independent). Their
  // absence would mean resolveMode()'s runtime branches were stripped and the http default can no
  // longer be trusted at runtime.
  const seams = [
    { label: 'runtime __AGG_MODE__ injection seam', ok: bundle.includes('__AGG_MODE__') },
    { label: 'runtime mock opt-in seam (constellation.aggMode)', ok: bundle.includes('constellation.aggMode') },
  ];
  for (const s of seams) console.log(`  [${s.ok ? 'ok' : 'FAIL'}] ${s.label}`);
  if (seams.some(s => !s.ok)) {
    fail('resolveMode() runtime-selection seams are missing from the runtime-default bundle — refusing to ship.');
  }
} else {
  console.log(`[assert-http-bundle] explicit VITE_AGG_MODE build — runtime seams intentionally DCE'd, skipping seam check.`);
}

// ── 3. RMCP-13 (TERM-624): the connector FIXTURE SERVER must not be in any shipped asset ──────
//
// `src/lib/rmcpFixtures.ts` is a mock server for the Connectors page. Its data is fabricated
// AUTHORIZATION data — which tools a connector can reach — so an operator shown it while making
// real scoping decisions would be misled about access in the most consequential place in the app.
// `rmcpClient.ts` therefore reaches it only through a dynamic import behind a literal
// `!import.meta.env.PROD` guard, which Vite folds away at build time.
//
// This asserts that folding actually happened, in EVERY emitted asset (a dynamic import that
// survived would land in its own chunk, not in index-*.js). Review round 1's point stands: the
// property has to be checked, not documented — a future top-level import would silently undo it,
// and this is what turns that into a failed build.
{
  const ROOT = join(fileURLToPath(new URL('.', import.meta.url)), '..');
  const ASSETS = join(ROOT, 'dist', 'assets');
  // Must match RMCP_FIXTURE_MARKER in src/lib/rmcpFixtures.ts. It is referenced in a thrown
  // message there, so a minifier cannot keep the module while dropping the string.
  const FIXTURE_MARKER = 'rmcp-fixture-server-must-never-ship';
  let offenders = [];
  try {
    offenders = readdirSync(ASSETS)
      .filter(f => f.endsWith('.js'))
      .filter(f => readFileSync(join(ASSETS, f), 'utf8').includes(FIXTURE_MARKER));
  } catch (e) {
    fail(e.message);
  }
  if (offenders.length > 0) {
    fail(`the RMCP connector fixture server is present in shipped asset(s): ${offenders.join(', ')} — `
      + `it must be dead-code-eliminated (see the !import.meta.env.PROD guard in src/lib/rmcpClient.ts). `
      + `Shipping it risks showing fabricated authorization data on the Connectors page.`);
  }
  console.log(`  [ok] RMCP connector fixture server absent from all ${readdirSync(ASSETS).filter(f => f.endsWith('.js')).length} shipped JS asset(s)`);
}

console.log('\n[assert-http-bundle] OK — resolved default is the real-backend (http) adapter.');
