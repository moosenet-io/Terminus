// Shared MINT low-confidence / small-sample caveat helpers — spec §6.2: "`low_confidence` and
// `n_samples <= 1` always render the ⚠ affordance + tooltip — never silently hidden." Centralized
// so `models.detail` and `models.compare` (CONST-22 review findings) show identical wording
// instead of each panel drifting its own copy.
import type { MintDimensionScore } from '../types/models';

export function isLowConfidenceScore(score: Pick<MintDimensionScore, 'low_confidence' | 'n'>): boolean {
  return score.low_confidence || score.n <= 1;
}

/** Tooltip body (raw value, ±std_dev, n) for the ⚠ affordance — used as a `title` attr. */
export function mintCaveatTooltip(score: MintDimensionScore): string {
  return `low confidence: raw=${score.raw.toFixed(2)}, ±${score.std_dev.toFixed(2)}, n=${score.n}`;
}
