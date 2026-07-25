#!/usr/bin/env node
// S127 TGUI2 (DATA-01): post-build guard — fail the build if the emitted SPA bundle cannot
// reach the http (real-backend) adapter, so a mock-only bundle can never ship silently again.
//
// The adapter default is http (see src/lib/aggregationClient.ts resolveMode), so a correct
// bundle always contains the http-default return AND the runtime opt-in seams. This asserts
// those survive minification. Run after `vite build` (see the `build:verify` npm script).
//
// Exits non-zero on failure — wire this into CI (constellation-updater / OCI image build) after
// the constellation-web build step.
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(fileURLToPath(new URL('.', import.meta.url)), '..');
const ASSETS = join(ROOT, 'dist', 'assets');

let bundle;
try {
  const name = readdirSync(ASSETS).find(f => /^index-.*\.js$/.test(f));
  if (!name) throw new Error('no index-*.js bundle found in dist/assets');
  bundle = readFileSync(join(ASSETS, name), 'utf8');
  console.log(`[assert-http-bundle] inspecting dist/assets/${name}`);
} catch (e) {
  console.error(`[assert-http-bundle] FAIL: ${e.message}`);
  process.exit(1);
}

// The http-default return must be reachable, and the runtime opt-in seams must be present —
// their absence would mean a build hard-wired to mock, or a resolveMode regression.
const checks = [
  { label: 'http-default return present', ok: /return"http"/.test(bundle) || /return\s*"http"/.test(bundle) },
  { label: 'runtime __AGG_MODE__ seam present', ok: bundle.includes('__AGG_MODE__') },
  { label: 'mock opt-in seam present (constellation.aggMode)', ok: bundle.includes('constellation.aggMode') },
];

const failed = checks.filter(c => !c.ok);
for (const c of checks) console.log(`  [${c.ok ? 'ok' : 'FAIL'}] ${c.label}`);

if (failed.length > 0) {
  console.error(`\n[assert-http-bundle] FAIL: the emitted bundle cannot reach the http adapter — refusing to ship a mock-only build.`);
  process.exit(1);
}
console.log('\n[assert-http-bundle] OK — bundle defaults to the real-backend (http) adapter.');
