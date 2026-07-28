// Shared MINT low-confidence / small-sample caveat helpers — spec §6.2: "`low_confidence` and
// `n_samples <= 1` always render the ⚠ affordance + tooltip — never silently hidden." Centralized
// so `models.compare` (and any future MINT-dimension consumer) show identical wording instead of
// each panel drifting its own copy. Typed against the real CGUI-08 `MintDimensionScore`
// (`types/mint.ts`, `raw`/`std_dev` nullable) — this file originally typed against the bespoke
// CONST-22 `types/models.ts` shape (non-nullable `raw`/`std_dev`), reconciled here.
import type { MintDimensionScore } from '../types/mint';

export function isLowConfidenceScore(score: Pick<MintDimensionScore, 'low_confidence' | 'n'>): boolean {
  return score.low_confidence || score.n <= 1;
}

/** Tooltip body (raw value, ±std_dev, n) for the ⚠ affordance — used as a `title` attr. Falls
 *  back to `—` for a null raw/std_dev (the real backend can omit either independently of
 *  `low_confidence`/`n`). */
export function mintCaveatTooltip(score: MintDimensionScore): string {
  const raw = score.raw == null ? '—' : score.raw.toFixed(2);
  const std = score.std_dev == null ? '—' : `±${score.std_dev.toFixed(2)}`;
  return `low confidence: raw=${raw}, ${std}, n=${score.n}`;
}
