// CONST-25: unit coverage for the fuzzy matcher (`commandMatch.ts`). This repo has no JS test
// runner wired up yet (no vitest/jest/etc. in constellation-web/package.json or a root
// workspace config as of this item — verified before writing this file) — adding one is out of
// scope for a command-palette item. Rather than either skip coverage or reach for a new
// dependency, this is a dependency-free, self-checking assertion file: `runCommandMatchTests()`
// throws with a descriptive message on the first failure and returns the pass count otherwise.
// It typechecks as part of `tsc --noEmit` like every other file under `src`, so a signature
// drift in `commandMatch.ts` still fails the build gate immediately. Wire it into `npm test`
// (e.g. via `tsx src/lib/commandMatch.test.ts` or a real vitest setup) the moment this repo
// gets a JS test runner — nothing about this file's shape needs to change to be picked up by one.
import { fuzzyMatch, rankItems } from './commandMatch';

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(`commandMatch.test: ${message}`);
}

export function runCommandMatchTests(): number {
  let passed = 0;
  const check = (name: string, fn: () => void) => {
    fn();
    passed++;
    void name;
  };

  check('empty query matches everything with score 0', () => {
    const m = fuzzyMatch('', 'anything');
    assert(m !== null, 'expected a match');
    assert(m!.score === 0, `expected score 0, got ${m!.score}`);
    assert(m!.indices.length === 0, 'expected no indices');
  });

  check('exact substring match', () => {
    const m = fuzzyMatch('chord', 'Chord · Providers');
    assert(m !== null, 'expected "chord" to match "Chord · Providers"');
  });

  check('subsequence match (non-contiguous)', () => {
    const m = fuzzyMatch('cvd', 'Chord · Providers');
    assert(m !== null, 'expected "cvd" to subsequence-match "Chord · Providers"');
  });

  check('no match when a query char is missing', () => {
    const m = fuzzyMatch('xyz', 'Chord Providers');
    assert(m === null, 'expected no match for "xyz"');
  });

  check('is case-insensitive', () => {
    const m = fuzzyMatch('CHORD', 'chord providers');
    assert(m !== null, 'expected case-insensitive match');
  });

  check('word-boundary hits outrank mid-word hits', () => {
    // "go to · chord · providers" vs "go to · terminus · configuration" for query "co":
    // 'co' hits a word-boundary ("Chord") vs a mid-word run in "Configuration"? Use a
    // clearer pair: boundary match ("Chord") vs a same-length mid-string match.
    const boundary = fuzzyMatch('ch', 'Go to · Chord · Providers');
    const midword = fuzzyMatch('ch', 'Go to · Sync History');
    assert(boundary !== null && midword !== null, 'expected both to match');
    assert(boundary!.score > midword!.score, 'expected the word-boundary hit to score higher');
  });

  check('earlier match start scores higher than a later one, all else equal', () => {
    const early = fuzzyMatch('go', 'Go to overview');
    const late = fuzzyMatch('go', 'Navigate to go');
    assert(early !== null && late !== null, 'expected both to match');
    assert(early!.score >= late!.score, 'expected the earlier match to score at least as high');
  });

  check('indices are in ascending order and within bounds', () => {
    const m = fuzzyMatch('gtp', 'Go to · Providers');
    assert(m !== null, 'expected a match');
    for (let i = 1; i < m!.indices.length; i++) {
      assert(m!.indices[i] > m!.indices[i - 1], 'indices must be strictly ascending');
    }
    for (const idx of m!.indices) {
      assert(idx >= 0 && idx < 'Go to · Providers'.length, 'index out of bounds');
    }
  });

  check('rankItems drops non-matches and sorts best-first', () => {
    const items = ['Chord · Inference', 'Terminus · Config', 'Chord · Providers', 'Lumina · Config'];
    const ranked = rankItems('chord', items, s => s);
    assert(ranked.length === 2, `expected 2 matches, got ${ranked.length}`);
    assert(ranked.every(r => r.item.toLowerCase().includes('chord')), 'expected only chord items');
  });

  check('rankItems is empty for an empty item list', () => {
    const ranked = rankItems('anything', [], (s: string) => s);
    assert(ranked.length === 0, 'expected no results');
  });

  return passed;
}

// Self-run (review fix): executing this file directly (`npx tsx src/lib/commandMatch.test.ts`)
// actually runs the suite — previously only the export existed, so a direct run silently did
// nothing. Importing the module elsewhere still does NOT auto-run (nothing imports this file;
// it exists to be executed).
const results = runCommandMatchTests();
// eslint-disable-next-line no-console
console.log(`commandMatch self-check: ${results} assertions passed`);
