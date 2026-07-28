#!/usr/bin/env node
// CGUI-01 — Design-system adherence lint (WARN mode, first pass; never blocks the build).
//
// This is the runnable equivalent of the DS-shipped `oxlintrc.adherence.json` (kept in the
// repo root as the canonical rule reference). oxlint 1.x does not yet implement the
// `no-restricted-syntax` rule that the DS ruleset is built on, so the three raw-value
// guards are enforced here directly instead:
//   1. no raw hex colors   — use a color token via var(--…)
//   2. no raw px literals   — use a spacing token via var(--…)
//   3. font-family must be Inter / JetBrains Mono only
//
// The DS component PROP ENUMS (Button.variant, Badge.tone, StatusPill.state, NodeBadge.kind,
// Card props) are already enforced at compile time by the TypeScript union types on each
// primitive, so they are not re-checked here.
//
// ALWAYS exits 0 — this surfaces adherence debt without failing CI. Flip `process.exitCode`
// to 1 in a later hardening pass once the existing warnings are burned down.

import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(fileURLToPath(new URL('.', import.meta.url)), '..');
const SRC = join(ROOT, 'src');

const HEX = /#[0-9a-fA-F]{3,8}\b/;
const PX = /\b\d+px\b/;
const FONT_FAMILY = /font-?[fF]amily\s*[:=]\s*['"`]?\s*(?!.*(?:Inter|JetBrains Mono|var\(|inherit|monospace|sans-serif))/;

const RULES = [
  { re: HEX, msg: 'raw hex color — use a color token via var(--…)' },
  { re: PX, msg: 'raw px literal — use a spacing token via var(--…)' },
  { re: FONT_FAMILY, msg: 'font-family not provided by the DS — use Inter or JetBrains Mono' },
];

function walk(dir) {
  const out = [];
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    const s = statSync(p);
    if (s.isDirectory()) out.push(...walk(p));
    else if (/\.(ts|tsx)$/.test(name)) out.push(p);
  }
  return out;
}

let warnings = 0;
for (const file of walk(SRC)) {
  const lines = readFileSync(file, 'utf8').split('\n');
  lines.forEach((line, i) => {
    const code = line.replace(/\/\/.*$/, ''); // ignore trailing line comments
    for (const { re, msg } of RULES) {
      if (re.test(code)) {
        console.warn(`  [warn] ${relative(ROOT, file)}:${i + 1} — ${msg}`);
        warnings++;
      }
    }
  });
}

console.warn(`\nadherence: ${warnings} warning(s) (WARN mode — build not blocked).`);
process.exitCode = 0;
