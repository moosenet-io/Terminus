// CGUI-10 (TERM #533): unit coverage for the MINT response→view-model normalizers, exercised
// against the SAME mock adapter the panels consume so the transforms are pinned to the real
// data-client contract. Logic-only (no jsdom).
import { describe, expect, it } from 'vitest';
import { mockAdapter } from '../../lib/aggregationClient';
import {
  radarFromCategory, heatmapFromCategory, boxFromCategory, failuresFromCategory,
  rankingFromCategory, metricsOfCategory, radarFromDimensions, heatmapFromLegacyMatrix,
  rankingFromDimensions, boxFromLegacy, failuresFromLegacy, radarFromLegacyMatrix,
  rankingFromLegacyMatrix,
} from './transforms';
import type { MintCategoryMatrixResponse, MintMatrixResponse } from '../../types/mint';

describe('newcat transforms (embedding_retrieval fixture)', () => {
  it('radarFromCategory yields one aligned value vector per model over the category metrics', async () => {
    const summary = await mockAdapter.mint.categorySummary('embedding_retrieval');
    const vm = radarFromCategory(summary);
    expect(vm.axes).toEqual(['ndcg_at_10', 'mrr', 'recall_at_10']);
    expect(vm.series.length).toBe(summary.models.length);
    for (const s of vm.series) {
      expect(s.values).toHaveLength(vm.axes.length);
      for (const v of s.values) { expect(v).toBeGreaterThanOrEqual(0); expect(v).toBeLessThanOrEqual(1); }
    }
  });

  it('heatmapFromCategory builds a dense model×metric cell map with raw + quality', async () => {
    const matrix = await mockAdapter.mint.categoryMatrix('embedding_retrieval');
    const vm = heatmapFromCategory(matrix);
    expect(vm.models.length).toBeGreaterThan(0);
    const m0 = vm.models[0];
    const met0 = vm.metrics[0];
    const cell = vm.cell[m0][met0];
    expect(cell.value).not.toBeNull();
    expect(cell.quality).toBeGreaterThanOrEqual(0);
    expect(cell.quality).toBeLessThanOrEqual(1);
  });

  it('boxFromCategory carries the 5-number summary + outliers through', async () => {
    const box = await mockAdapter.mint.categoryBox('embedding_retrieval');
    const vm = boxFromCategory(box);
    expect(vm.groups.length).toBeGreaterThan(0);
    const g = vm.groups[0];
    expect(g.q1).toBeLessThanOrEqual(g.median);
    expect(g.median).toBeLessThanOrEqual(g.q3);
    expect(Array.isArray(g.outliers)).toBe(true);
  });

  it('rankingFromCategory sorts models best-capability-first', async () => {
    const summary = await mockAdapter.mint.categorySummary('reranking');
    const primary = metricsOfCategory(summary)[0];
    const rows = rankingFromCategory(summary, primary);
    expect(rows.length).toBe(summary.models.length);
    for (let i = 1; i < rows.length; i++) expect(rows[i - 1].score).toBeGreaterThanOrEqual(rows[i].score);
  });

  it('failuresFromCategory preserves classes + per-model counts', async () => {
    const f = await mockAdapter.mint.categoryFailures('tool_routing');
    const vm = failuresFromCategory(f);
    expect(vm.classes).toContain('ok');
    expect(vm.models.length).toBeGreaterThan(0);
    expect(vm.models[0].total).toBeGreaterThan(0);
  });
});

describe('legacy + persona transforms', () => {
  it('radarFromDimensions uses server norm values in [0,1]', async () => {
    const dims = await mockAdapter.mint.dimensions();
    const vm = radarFromDimensions(dims);
    expect(vm.axes.length).toBeGreaterThan(0);
    expect(vm.series.length).toBe(dims.models.length);
    for (const s of vm.series) for (const v of s.values) { expect(v).toBeGreaterThanOrEqual(0); expect(v).toBeLessThanOrEqual(1); }
  });

  it('heatmapFromLegacyMatrix keys columns by test_type/task_category', async () => {
    const matrix = await mockAdapter.mint.matrix();
    const vm = heatmapFromLegacyMatrix(matrix);
    expect(vm.models.length).toBeGreaterThan(0);
    expect(vm.metrics.some(m => m.includes('/'))).toBe(true);
  });

  it('rankingFromDimensions ranks by mean dimension score', async () => {
    const dims = await mockAdapter.mint.dimensions();
    const rows = rankingFromDimensions(dims);
    for (let i = 1; i < rows.length; i++) expect(rows[i - 1].score).toBeGreaterThanOrEqual(rows[i].score);
  });

  it('boxFromLegacy + failuresFromLegacy pass fleet data through', async () => {
    const box = await mockAdapter.mint.box();
    expect(boxFromLegacy(box, 'total_time_ms').groups.length).toBeGreaterThan(0);
    const f = await mockAdapter.mint.failures();
    expect(failuresFromLegacy(f).classes.length).toBeGreaterThan(0);
  });
});

describe('legacy suite scoping (FIX 1 — tabs must differ per suite)', () => {
  it('heatmapFromLegacyMatrix scopes to a test_type and drops the other suites’ columns', async () => {
    const matrix = await mockAdapter.mint.matrix();
    const coder = heatmapFromLegacyMatrix(matrix, 'coder');
    const assistant = heatmapFromLegacyMatrix(matrix, 'assistant');
    // scoped columns are the bare task_category (no `test_type/` prefix)
    expect(coder.metrics.every(m => !m.includes('/'))).toBe(true);
    expect(coder.metrics).toContain('code_generation');
    expect(assistant.metrics).toContain('embedding_retrieval');
    // the two suites present DIFFERENT columns — this is the bug the review caught
    expect(coder.metrics).not.toEqual(assistant.metrics);
  });

  it('radar/ranking from the legacy matrix differ between suites', async () => {
    const matrix = await mockAdapter.mint.matrix();
    const coderRadar = radarFromLegacyMatrix(matrix, 'coder');
    const assistantRadar = radarFromLegacyMatrix(matrix, 'assistant');
    expect(coderRadar.axes).not.toEqual(assistantRadar.axes);
    // coder scope only includes models with a coder cell
    const coderRank = rankingFromLegacyMatrix(matrix, 'coder');
    expect(coderRank.every(r => r.score >= 0 && r.score <= 1)).toBe(true);
  });

  it('an unmatched test_type yields an empty (not aggregate) heatmap', async () => {
    const matrix = await mockAdapter.mint.matrix();
    const context = heatmapFromLegacyMatrix(matrix, 'context');
    expect(context.models).toEqual([]);
    expect(context.metrics).toEqual([]);
  });
});

describe('null-vs-zero cell coloring (FIX 2)', () => {
  it('heatmapFromCategory maps a null mean to quality null, not 0', () => {
    const m: MintCategoryMatrixResponse = {
      models: ['m1'],
      columns: ['ndcg_at_10'],
      cells: [{ model: 'm1', metric: 'ndcg_at_10', dimension: 'embedding_retrieval', mean: null as unknown as number, n: 0, low_confidence: false, last_run_at: '' }],
    };
    const vm = heatmapFromCategory(m);
    expect(vm.cell.m1.ndcg_at_10.value).toBeNull();
    expect(vm.cell.m1.ndcg_at_10.quality).toBeNull();
  });

  it('heatmapFromLegacyMatrix maps a not-run (null pass_rate) cell to quality null', async () => {
    const matrix: MintMatrixResponse = await mockAdapter.mint.matrix();
    const vm = heatmapFromLegacyMatrix(matrix, 'coder');
    // the mock ships a not_run bge-m3 coder/code_generation cell (pass_rate null)
    const notRun = vm.cell['bge-m3']?.['code_generation'];
    if (notRun) {
      expect(notRun.value).toBeNull();
      expect(notRun.quality).toBeNull();
    }
    // a genuine 0.0 would still be quality 0 — assert null is distinct from a real score
    const scored = vm.cell['qwen2.5-coder:32b']?.['code_generation'];
    expect(scored?.quality).not.toBeNull();
  });
});

describe('fail-open on empty / null inputs', () => {
  it('every transform returns an empty VM rather than throwing', () => {
    expect(radarFromCategory(null).axes).toEqual([]);
    expect(heatmapFromCategory(null).models).toEqual([]);
    expect(boxFromCategory(null).groups).toEqual([]);
    expect(failuresFromCategory(null).models).toEqual([]);
    expect(rankingFromCategory(null, null)).toEqual([]);
    expect(radarFromDimensions(null).series).toEqual([]);
    expect(heatmapFromLegacyMatrix(null).models).toEqual([]);
    expect(heatmapFromLegacyMatrix(null, 'coder').models).toEqual([]);
    expect(radarFromLegacyMatrix(null, 'coder').axes).toEqual([]);
    expect(rankingFromLegacyMatrix(null, 'coder')).toEqual([]);
    expect(rankingFromDimensions(null)).toEqual([]);
    expect(metricsOfCategory(null)).toEqual([]);
  });
});
