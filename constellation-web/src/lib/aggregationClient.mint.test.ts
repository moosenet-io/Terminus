// CGUI-08 (TERM #531): unit coverage for the Models/MINT data-client method group on the
// MOCK adapter (VITE_AGG_MODE default). These assert the mock fixtures match the shapes the
// httpAdapter types against (`../types/mint`) so CGUI-09/10 develop offline against the same
// contract the real endpoints serve.
import { describe, expect, it } from 'vitest';
import { mockAdapter } from './aggregationClient';
import { MINT_CATEGORIES } from '../types/mint';

describe('CGUI-08 models data-client (mock adapter)', () => {
  it('models.list() returns the canned fleet list', async () => {
    const res = await mockAdapter.models.list();
    expect(res.total).toBe(res.models.length);
    expect(res.models.length).toBeGreaterThan(0);
    expect(res.models[0]).toHaveProperty('coverage');
    expect(res.models[0].coverage).toHaveProperty('coder');
  });

  it('models.list() honors the scope=fleet filter', async () => {
    const all = await mockAdapter.models.list();
    const fleet = await mockAdapter.models.list({ scope: 'fleet' });
    expect(fleet.models.every(m => m.in_current_fleet)).toBe(true);
    expect(fleet.models.length).toBeLessThanOrEqual(all.models.length);
  });

  it('models.list() honors the serving filter', async () => {
    const serving = await mockAdapter.models.list({ serving: true });
    expect(serving.models.every(m => m.serving_now === true)).toBe(true);
  });

  it('models.model(name) returns detail for a known model', async () => {
    const detail = await mockAdapter.models.model('qwen2.5-coder:32b');
    expect(detail.identity).not.toBeNull();
    expect(detail.identity?.family).toBeDefined();
    expect(Array.isArray(detail.serving)).toBe(true);
    expect(detail.catalog?.cells.length).toBeGreaterThan(0);
  });

  it('models.model(name) rejects an unknown model (backend 404 parity)', async () => {
    await expect(mockAdapter.models.model('does-not-exist')).rejects.toThrow(/404/);
  });
});

describe('CGUI-08 MINT data-client — legacy views (mock adapter)', () => {
  it('summary() has the runs breakdown + epoch', async () => {
    const s = await mockAdapter.mint.summary();
    expect(s.runs.total).toBe(s.runs.code + s.runs.context + s.runs.agent);
    expect(typeof s.epoch).toBe('string');
  });

  it('dimensions() returns 8 axes with per-model scores', async () => {
    const d = await mockAdapter.mint.dimensions();
    expect(d.dimensions).toHaveLength(8);
    expect(d.models[0].scores).toHaveLength(8);
    expect(d.fleet_median).toHaveLength(8);
  });

  it('matrix() returns models/columns/cells', async () => {
    const m = await mockAdapter.mint.matrix();
    expect(m.columns[0]).toHaveProperty('test_type');
    expect(m.cells[0].col).toHaveProperty('task_category');
  });

  it('box() groups carry the five-number summary + low_n flag', async () => {
    const b = await mockAdapter.mint.box();
    const g = b.groups[0];
    expect(g.min).toBeLessThanOrEqual(g.q1);
    expect(g.q1).toBeLessThanOrEqual(g.median);
    expect(g.median).toBeLessThanOrEqual(g.q3);
    expect(g.q3).toBeLessThanOrEqual(g.max);
    expect(typeof g.low_n).toBe('boolean');
  });

  it('languageStats() rows include the server-computed point_size_px', async () => {
    const ls = await mockAdapter.mint.languageStats();
    expect(ls.rows[0].point_size_px).toBeGreaterThanOrEqual(8);
    expect(ls.rows[0].point_size_px).toBeLessThanOrEqual(24);
  });

  it('failures() classes end with an "other" fold', async () => {
    const f = await mockAdapter.mint.failures();
    expect(f.classes[f.classes.length - 1]).toBe('other');
    expect(f.models[0].total_runs).toBeGreaterThan(0);
  });

  it('contextProfiles() returns per-model tier arrays', async () => {
    const cp = await mockAdapter.mint.contextProfiles();
    expect(cp.models[0].tiers.length).toBeGreaterThan(0);
    expect(cp.models[0].tiers[0]).toHaveProperty('throughput_tok_per_sec');
  });

  it('activity() returns day buckets + epoch markers', async () => {
    const a = await mockAdapter.mint.activity();
    expect(a.days.length).toBeGreaterThan(0);
    expect(a.days[0]).toHaveProperty('code');
    expect(a.epochs.length).toBeGreaterThan(0);
  });

  it('runs(suite=<new category>) shapes assistant rows as runs', async () => {
    const r = await mockAdapter.mint.runs({ suite: 'embedding_retrieval' });
    expect(r.total).toBe(r.runs.length);
    expect(r.runs.length).toBeGreaterThan(0);
    expect(r.runs[0].metric).toBeDefined();
  });

  it('runs() accepts legacy suites, categories, and aliases', async () => {
    for (const suite of ['code', 'context', 'agent', 'reranking', 'vision_qa', 'stt', 'asr_transcription'] as const) {
      await expect(mockAdapter.mint.runs({ suite })).resolves.toBeDefined();
    }
  });

  it('runs() rejects a truly-unknown suite (backend 400 parity)', async () => {
    // @ts-expect-error — deliberately invalid suite to exercise the guard
    await expect(mockAdapter.mint.runs({ suite: 'not_a_category' })).rejects.toThrow(/400/);
  });
});

describe('CGUI-08 MINT data-client — determinism (mock adapter)', () => {
  it('repeated calls return byte-identical payloads (no request-time Date.now)', async () => {
    const [a, b] = await Promise.all([
      mockAdapter.mint.categorySummary('embedding_retrieval'),
      mockAdapter.mint.categorySummary('embedding_retrieval'),
    ]);
    expect(JSON.stringify(a)).toBe(JSON.stringify(b));
    const [m1, m2] = await Promise.all([
      mockAdapter.models.model('qwen2.5-coder:32b'),
      mockAdapter.models.model('qwen2.5-coder:32b'),
    ]);
    expect(JSON.stringify(m1)).toBe(JSON.stringify(m2));
    const [s1, s2] = await Promise.all([mockAdapter.mint.summary(), mockAdapter.mint.summary()]);
    expect(JSON.stringify(s1)).toBe(JSON.stringify(s2));
  });
});

describe('CGUI-08 MINT data-client — per-category views (mock adapter)', () => {
  it('every one of the 8 categories has a non-empty summary', async () => {
    for (const cat of MINT_CATEGORIES) {
      const s = await mockAdapter.mint.categorySummary(cat);
      expect(s.models.length, `summary for ${cat}`).toBeGreaterThan(0);
      expect(s.models[0].metrics.length, `metrics for ${cat}`).toBeGreaterThan(0);
    }
  });

  it('categoryMatrix() cell count = models × metrics', async () => {
    const m = await mockAdapter.mint.categoryMatrix('reranking');
    expect(m.cells.length).toBe(m.models.length * m.columns.length);
  });

  it('categoryBox() defaults to the first metric and honors an explicit one', async () => {
    const def = await mockAdapter.mint.categoryBox('embedding_retrieval');
    expect(def.metric).toBe('ndcg_at_10');
    const explicit = await mockAdapter.mint.categoryBox('embedding_retrieval', 'mrr');
    expect(explicit.metric).toBe('mrr');
    // an unknown metric fails open to empty groups (parity with shape_newcat_box)
    const empty = await mockAdapter.mint.categoryBox('embedding_retrieval', 'nope');
    expect(empty.groups).toEqual([]);
  });

  it('categoryDimensions() lists (dimension, metric) pairs', async () => {
    const d = await mockAdapter.mint.categoryDimensions('tts');
    expect(d.dimensions.length).toBeGreaterThan(0);
    expect(d.dimensions[0]).toHaveProperty('metric');
  });

  it('categoryFailures() splits low_confidence vs ok', async () => {
    const f = await mockAdapter.mint.categoryFailures('tool_routing');
    expect(f.classes).toEqual(['low_confidence', 'ok']);
    const m = f.models[0];
    expect(m.counts.low_confidence + m.counts.ok).toBe(m.total_runs);
  });

  it('the vision_qa / stt aliases resolve to their canonical category', async () => {
    const vision = await mockAdapter.mint.categorySummary('vision_qa');
    expect(vision.models[0].metrics[0].dimension).toBe('image_parsing');
    const stt = await mockAdapter.mint.categorySummary('stt');
    expect(stt.models[0].metrics[0].dimension).toBe('voice_transcription');
  });

  it('an unknown category rejects (backend 400 parity)', async () => {
    // @ts-expect-error — deliberately passing an invalid category to exercise the guard
    await expect(mockAdapter.mint.categorySummary('not_a_category')).rejects.toThrow(/400/);
  });
});
