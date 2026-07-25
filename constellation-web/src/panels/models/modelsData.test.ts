// CGUI-09 (TERM #532): unit coverage for the Models module's pure derivations. Runs against
// the CGUI-08 mock adapter fixtures (deterministic — no Date.now/RNG), so the assertions are
// stable snapshots of the same contract the http adapter serves live.
import { describe, expect, it } from 'vitest';
import { mockAdapter } from '../../lib/aggregationClient';
import {
  deriveServingState,
  deriveCostTier,
  coverageBadges,
  matchesQuery,
  buildCategoryRadar,
  fmtPct,
  fmtGb,
} from './modelsData';
import type { ModelListEntry } from '../../types/mint';

async function firstOf(name: string): Promise<ModelListEntry> {
  const res = await mockAdapter.models.list();
  const entry = res.models.find(m => m.model_name === name);
  if (!entry) throw new Error(`fixture missing ${name}`);
  return entry;
}

describe('CGUI-09 deriveServingState', () => {
  it('a live serve is hot + pulsing', async () => {
    const d = deriveServingState(await firstOf('qwen2.5-coder:32b')); // serving_now: true
    expect(d.state).toBe('hot');
    expect(d.pulse).toBe(true);
    expect(d.label).toBe('Serving');
  });

  it('an in-fleet cold model is cold, not pulsing', async () => {
    const d = deriveServingState(await firstOf('bge-m3')); // in_fleet, serving_now: false
    expect(d.state).toBe('cold');
    expect(d.pulse).toBe(false);
  });

  it('a non-fleet brochure candidate is idle', async () => {
    const d = deriveServingState(await firstOf('flux.1-schnell')); // not in fleet
    expect(d.state).toBe('idle');
    expect(d.label).toBe('Candidate');
  });
});

describe('CGUI-09 deriveCostTier', () => {
  it('tiers by VRAM footprint', async () => {
    expect(deriveCostTier(await firstOf('qwen2.5-coder:32b')).label).toBe('M'); // 21.5 GB
    expect(deriveCostTier(await firstOf('bge-m3')).label).toBe('XS'); // 2.1 GB
    expect(deriveCostTier(await firstOf('flux.1-schnell')).label).toBe('M'); // 16 GB (12–24 → M)
  });

  it('falls back to a neutral tier when nothing is known', () => {
    const bare = { vram_gb: null, params_b: null } as ModelListEntry;
    expect(deriveCostTier(bare)).toEqual({ label: '—', tone: 'neutral' });
  });
});

describe('CGUI-09 coverageBadges', () => {
  it('lists only the set capability flags, in order', async () => {
    const badges = coverageBadges(await firstOf('qwen2.5-coder:32b'));
    expect(badges.map(b => b.key)).toEqual(['coder', 'assistant', 'serving']);
  });

  it('is empty for a model that covers nothing', async () => {
    expect(coverageBadges(await firstOf('flux.1-schnell'))).toEqual([]);
  });
});

describe('CGUI-09 matchesQuery', () => {
  it('matches name/family/category case-insensitively; empty matches all', async () => {
    const e = await firstOf('qwen2.5-coder:32b');
    expect(matchesQuery(e, '')).toBe(true);
    expect(matchesQuery(e, 'QWEN')).toBe(true);
    expect(matchesQuery(e, 'code')).toBe(true); // family qwen2.5-coder
    expect(matchesQuery(e, 'llama')).toBe(false);
  });
});

describe('CGUI-09 buildCategoryRadar', () => {
  it('folds catalog cells into sorted per-category spokes', async () => {
    const detail = await mockAdapter.models.model('qwen2.5-coder:32b');
    const radar = buildCategoryRadar(detail);
    expect(radar.hasData).toBe(true);
    expect(radar.axes.length).toBeGreaterThan(0);
    expect(radar.axes[0]).toHaveProperty('category');
    expect(radar.axes[0]).toHaveProperty('score');
    // deterministic sort by category
    const cats = radar.axes.map(a => a.category);
    expect([...cats].sort((a, b) => a.localeCompare(b))).toEqual(cats);
  });

  it('fail-open: null detail or no scored cells → hasData false, empty axes', () => {
    expect(buildCategoryRadar(null)).toEqual({ axes: [], hasData: false });
  });

  it('drops cells whose pass_rate is null', () => {
    const detail = {
      identity: null, brochure: null, serving: [], operational: null,
      catalog: {
        card: { model_name: 'x', quant: null, in_current_fleet: false, serving: null, not_run_count: 0, stale_count: 0, refreshed_at: null },
        cells: [
          { test_type: 'coder', task_category: 'a', quant: null, status: 'run', pass_rate: 0.5, n_samples: 1, score_stddev: null, low_confidence: false, last_run_at: null, harness_version: null },
          { test_type: 'coder', task_category: 'b', quant: null, status: 'not_run', pass_rate: null, n_samples: null, score_stddev: null, low_confidence: false, last_run_at: null, harness_version: null },
        ],
      },
    };
    const radar = buildCategoryRadar(detail);
    expect(radar.axes.map(a => a.category)).toEqual(['a']);
  });
});

describe('CGUI-09 formatters', () => {
  it('fmtPct / fmtGb render nullable values as an em-dash', () => {
    expect(fmtPct(0.78)).toBe('78%');
    expect(fmtPct(null)).toBe('—');
    expect(fmtGb(21.5)).toBe('21.5 GB');
    expect(fmtGb(null)).toBe('—');
  });
});
