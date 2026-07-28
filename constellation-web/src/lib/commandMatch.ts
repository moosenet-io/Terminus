// CONST-25 (§3.2): the CommandPalette's own fuzzy matcher — a subsequence matcher with a
// word-boundary bonus, deliberately hand-rolled (zero new deps, per the item's grep gate).
// Exported as small pure functions so they're independently testable even though this repo has
// no test runner wired up yet (no vitest/jest in constellation-web/package.json as of CONST-25 —
// see the colocated `commandMatch.test.ts` for assertions a future test-infra item can pick up).

/** One scored match against a query, or `null` if the query isn't a subsequence of the target. */
export interface MatchResult {
  /** Higher is better. Rewards: earlier match start, contiguous runs, and word-boundary hits
   *  (start of string, or right after a space/`-`/`_`/`/`/`.`). */
  score: number;
  /** Index positions in `target` that matched a query character — for highlight rendering. */
  indices: number[];
}

function isWordBoundary(target: string, index: number): boolean {
  if (index === 0) return true;
  const prev = target[index - 1];
  return prev === ' ' || prev === '-' || prev === '_' || prev === '/' || prev === '.';
}

/**
 * Case-insensitive subsequence match: every character of `query`, in order, must appear
 * somewhere in `target` (not necessarily contiguous). Returns `null` on no match.
 *
 * Scoring (higher = better, used to rank/sort hits, NOT a percentage):
 *  - +10 per matched character that lands on a word boundary
 *  - +3 per matched character that immediately continues the previous match (contiguous run)
 *  - +1 per matched character otherwise
 *  - -1 per character of `target` gap before the match starts (rewards earlier matches)
 */
export function fuzzyMatch(query: string, target: string): MatchResult | null {
  const q = query.trim().toLowerCase();
  if (q.length === 0) return { score: 0, indices: [] };
  const t = target.toLowerCase();

  const indices: number[] = [];
  let score = 0;
  let qi = 0;
  let lastMatchIndex = -1;
  let firstMatchIndex = -1;

  for (let ti = 0; ti < t.length && qi < q.length; ti++) {
    if (t[ti] !== q[qi]) continue;
    if (firstMatchIndex === -1) firstMatchIndex = ti;
    if (isWordBoundary(t, ti)) score += 10;
    else if (lastMatchIndex !== -1 && ti === lastMatchIndex + 1) score += 3;
    else score += 1;
    indices.push(ti);
    lastMatchIndex = ti;
    qi++;
  }

  if (qi < q.length) return null; // not every query char was found, in order
  score -= firstMatchIndex; // small penalty for a late start
  return { score, indices };
}

/** One item ranked against a query. */
export interface RankedItem<T> {
  item: T;
  match: MatchResult;
}

/**
 * Filters + ranks a list of items by fuzzy score against `getText(item)`, best first. Items with
 * no match are dropped. Stable for equal scores (preserves input order) since `Array.prototype.sort`
 * is a stable sort in all engines this app targets (ES2020+).
 */
export function rankItems<T>(query: string, items: readonly T[], getText: (item: T) => string): RankedItem<T>[] {
  const ranked: RankedItem<T>[] = [];
  for (const item of items) {
    const match = fuzzyMatch(query, getText(item));
    if (match) ranked.push({ item, match });
  }
  ranked.sort((a, b) => b.match.score - a.match.score);
  return ranked;
}
