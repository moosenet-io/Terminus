// S127 TGUI2 (DATA-04): the Roster's total/pagination fix, at the data-client seam the panel
// consumes. The RosterPanel now reports the server `total` (full roster scale) for its "Models"
// metric — not `rows.length` (the page size) — and pages the full set via limit/offset. These
// assert the adapter contract that fix relies on: `total` is the full filtered count regardless
// of page size, and `offset`/`limit` return the correct slice.
import { describe, it, expect } from 'vitest';
import { mockAdapter } from '../../lib/aggregationClient';

describe('models.list pagination contract (DATA-04)', () => {
  it('reports total as the full filtered count, independent of the returned page size', async () => {
    const full = await mockAdapter.models.list();
    const firstOfPageOne = await mockAdapter.models.list({ limit: 1, offset: 0 });
    // total is the whole roster, even though only one row came back — the bug was reporting the
    // page length (1) as the count instead of this.
    expect(firstOfPageOne.total).toBe(full.total);
    expect(firstOfPageOne.models.length).toBe(1);
    expect(firstOfPageOne.total).toBeGreaterThan(firstOfPageOne.models.length);
  });

  it('offset returns a distinct, non-overlapping later page', async () => {
    const page0 = await mockAdapter.models.list({ limit: 1, offset: 0 });
    const page1 = await mockAdapter.models.list({ limit: 1, offset: 1 });
    expect(page1.total).toBe(page0.total);
    expect(page1.models[0]?.model_name).not.toBe(page0.models[0]?.model_name);
  });

  it('an offset past the end yields an empty page but the true total', async () => {
    const full = await mockAdapter.models.list();
    const beyond = await mockAdapter.models.list({ limit: 50, offset: full.total + 10 });
    expect(beyond.models).toHaveLength(0);
    expect(beyond.total).toBe(full.total);
  });
});
