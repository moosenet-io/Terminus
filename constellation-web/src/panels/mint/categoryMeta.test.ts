// CGUI-10 (TERM #533): unit coverage for the MINT category taxonomy + metric-scale helpers.
// Logic-only (no jsdom) — same convention as the other *.test.ts suites.
import { describe, expect, it } from 'vitest';
import {
  MINT_CATEGORY_META, categoryById, DEFAULT_CATEGORY_ID, metricUnitScore, metricLabel,
  formatMetricValue, LOWER_IS_BETTER,
} from './categoryMeta';
import { MINT_CATEGORIES } from '../../types/mint';

describe('MINT_CATEGORY_META', () => {
  it('covers the 8 new categories plus code/context/agent + persona (12 total)', () => {
    expect(MINT_CATEGORY_META).toHaveLength(12);
    const ids = MINT_CATEGORY_META.map(c => c.id);
    // every new MINT category is present, keyed by its canonical clientKey
    for (const cat of MINT_CATEGORIES) expect(ids).toContain(cat);
    for (const legacy of ['code', 'context', 'agent', 'persona']) expect(ids).toContain(legacy);
  });

  it('every newcat carries a clientKey and every legacy suite a legacySuite', () => {
    for (const c of MINT_CATEGORY_META) {
      if (c.kind === 'newcat') expect(c.clientKey).toBeTruthy();
      if (c.kind === 'legacy') expect(c.legacySuite).toBeTruthy();
      if (c.kind === 'persona') { expect(c.clientKey).toBeUndefined(); expect(c.legacySuite).toBeUndefined(); }
    }
  });

  it('categoryById resolves and DEFAULT is the first entry', () => {
    expect(categoryById(DEFAULT_CATEGORY_ID)?.id).toBe(DEFAULT_CATEGORY_ID);
    expect(categoryById('nope')).toBeUndefined();
    expect(DEFAULT_CATEGORY_ID).toBe(MINT_CATEGORY_META[0].id);
  });
});

describe('metricUnitScore', () => {
  it('treats 0–1 higher-is-better metrics as their raw clamped value', () => {
    expect(metricUnitScore('ndcg_at_10', 0.82)).toBeCloseTo(0.82);
    expect(metricUnitScore('accuracy', 1.4)).toBe(1); // clamped
    expect(metricUnitScore('f1', -0.2)).toBe(0);
  });

  it('inverts error rates so lower is a higher capability', () => {
    expect(LOWER_IS_BETTER.has('wer')).toBe(true);
    expect(metricUnitScore('wer', 0.1)).toBeCloseTo(0.9);
    expect(metricUnitScore('cer', 0.05)).toBeCloseTo(0.95);
    // a better (lower) WER outranks a worse (higher) one
    expect(metricUnitScore('wer', 0.05)).toBeGreaterThan(metricUnitScore('wer', 0.2));
  });

  it('scales mos by /5 and aesthetic_score by /10', () => {
    expect(metricUnitScore('mos', 4.5)).toBeCloseTo(0.9);
    expect(metricUnitScore('aesthetic_score', 6)).toBeCloseTo(0.6);
  });

  it('decays latency/time metrics (lower = better) monotonically', () => {
    const fast = metricUnitScore('total_time_ms', 1000);
    const slow = metricUnitScore('total_time_ms', 8000);
    expect(fast).toBeGreaterThan(slow);
    expect(fast).toBeLessThanOrEqual(1);
    expect(slow).toBeGreaterThanOrEqual(0);
  });

  it('returns 0 for null/NaN rather than throwing', () => {
    expect(metricUnitScore('ndcg_at_10', null)).toBe(0);
    expect(metricUnitScore('ndcg_at_10', undefined)).toBe(0);
    expect(metricUnitScore('ndcg_at_10', NaN)).toBe(0);
  });
});

describe('metricLabel / formatMetricValue', () => {
  it('keeps known acronyms and titleizes the rest', () => {
    expect(metricLabel('ndcg_at_10')).toBe('nDCG@10');
    expect(metricLabel('description_quality')).toBe('Description Quality');
  });

  it('formats ms with a unit and null as an em dash', () => {
    expect(formatMetricValue('total_time_ms', 4200)).toBe('4,200 ms');
    expect(formatMetricValue('ndcg_at_10', 0.8199)).toBe('0.82');
    expect(formatMetricValue('ndcg_at_10', null)).toBe('—');
  });
});
