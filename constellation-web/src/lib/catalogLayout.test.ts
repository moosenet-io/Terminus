// MGUI-18: the sizing rules that silently break the Muse catalog pages if they regress.
import { describe, it, expect } from 'vitest';
import type { PanelDescriptor } from './moduleRegistry';
import {
  fluidBodyHeight,
  cardTrackPx,
  cardGridTemplate,
  clampCardSize,
  cardSizeLabel,
  contentMaxWidth,
  CARD_SIZE_DEFAULT,
  CARD_SIZE_MAX,
  CARD_SIZE_MIN,
  CARD_SIZE_SCALE,
  CARD_SIZE_STEPS,
  CARD_SIZE_LABELS,
  TRACK_MAX_PX,
  TRACK_MIN_PX,
  CATALOG_TRACK_BASE,
} from './catalogLayout';

describe('fluidBodyHeight', () => {
  // The whole point of the item: the body must be a function of the VIEWPORT, not a
  // constant. A value with no viewport unit in it is the bug this replaced.
  it('tracks the viewport rather than being a fixed height', () => {
    const h = fluidBodyHeight({ min: 320, max: 1400, reserve: 150 });
    expect(h).toContain('100dvh');
    expect(h).toBe('clamp(320px, calc(100dvh - 150px), 1400px)');
  });

  // dvh, not vh: on mobile `vh` is pinned to the address-bar-hidden viewport, so a
  // `100vh - reserve` box overflows behind the collapsed address bar.
  it('uses dynamic viewport units so mobile browser chrome does not clip the body', () => {
    expect(fluidBodyHeight({ min: 100, max: 200, reserve: 10 })).not.toMatch(/\d+vh/);
    expect(fluidBodyHeight({ min: 100, max: 200, reserve: 10 })).toContain('dvh');
  });

  it('keeps a floor and a ceiling so an ultraportable is not cramped nor an ultrawide stretched', () => {
    const h = fluidBodyHeight({ min: 280, max: 900, reserve: 380 });
    expect(h.startsWith('clamp(280px,')).toBe(true);
    expect(h.endsWith('900px)')).toBe(true);
  });

  // A backwards clamp is silently resolved to the min by CSS; normalise where it is visible.
  it('normalises a max below the min instead of emitting a backwards clamp', () => {
    expect(fluidBodyHeight({ min: 500, max: 100, reserve: 0 })).toBe(
      'clamp(500px, calc(100dvh - 0px), 500px)',
    );
  });

  // A negative reserve would ADD height to the viewport — never intended by any caller.
  it('never adds height back to the viewport via a negative reserve', () => {
    expect(fluidBodyHeight({ min: 10, max: 20, reserve: -400 })).toBe(
      'clamp(10px, calc(100dvh - 0px), 20px)',
    );
  });

  it('emits integral pixel lengths', () => {
    expect(fluidBodyHeight({ min: 320.4, max: 900.6, reserve: 150.5 })).toBe(
      'clamp(320px, calc(100dvh - 151px), 901px)',
    );
  });
});

describe('clampCardSize', () => {
  // The default must be the identity multiplier — an operator who never touches the slider
  // must see the grid exactly as it shipped, not a silently-resized one.
  it('defaults to the step whose multiplier is exactly 1', () => {
    expect(CARD_SIZE_SCALE[CARD_SIZE_DEFAULT]).toBe(1);
    expect(clampCardSize(undefined)).toBe(CARD_SIZE_DEFAULT);
    expect(clampCardSize(null)).toBe(CARD_SIZE_DEFAULT);
  });

  it('accepts every valid step unchanged', () => {
    for (let i = CARD_SIZE_MIN; i <= CARD_SIZE_MAX; i++) expect(clampCardSize(i)).toBe(i);
  });

  // A range input hands back a string; a localStorage round-trip may too.
  it('parses a numeric string, as a range input and localStorage both produce', () => {
    expect(clampCardSize('0')).toBe(0);
    expect(clampCardSize('4')).toBe(4);
  });

  // Clamped, NOT reset: shrinking the scale in a future build should degrade a stored 5 to
  // the new top step rather than throwing away the operator's stated preference.
  it('clamps an out-of-range step to the nearest end rather than resetting it', () => {
    expect(clampCardSize(-3)).toBe(CARD_SIZE_MIN);
    expect(clampCardSize(99)).toBe(CARD_SIZE_MAX);
  });

  it('falls back to the default for values that are not numbers at all', () => {
    expect(clampCardSize('wide')).toBe(CARD_SIZE_DEFAULT);
    expect(clampCardSize(NaN)).toBe(CARD_SIZE_DEFAULT);
    expect(clampCardSize(Infinity)).toBe(CARD_SIZE_DEFAULT);
    expect(clampCardSize({})).toBe(CARD_SIZE_DEFAULT);
  });

  it('has one label and one multiplier per step', () => {
    expect(CARD_SIZE_LABELS).toHaveLength(CARD_SIZE_STEPS.length);
    expect(CARD_SIZE_SCALE).toHaveLength(CARD_SIZE_STEPS.length);
  });
});

describe('cardTrackPx', () => {
  // The default step must reproduce the shipped track width for every panel, to the pixel.
  // Asserted against the SAME constants the panels render from, so this cannot pass while
  // a panel has quietly drifted to a different base.
  it('reproduces each panel’s shipped track width at the default step', () => {
    expect(cardTrackPx(CARD_SIZE_DEFAULT, CATALOG_TRACK_BASE.poster)).toBe(112);
    expect(cardTrackPx(CARD_SIZE_DEFAULT, CATALOG_TRACK_BASE.discover)).toBe(120);
    expect(cardTrackPx(CARD_SIZE_DEFAULT, CATALOG_TRACK_BASE.searchResult)).toBe(132);
    expect(cardTrackPx(CARD_SIZE_DEFAULT, CATALOG_TRACK_BASE.provider)).toBe(220);
  });

  // "increase or decrease the media cards grid so i can fit more or less items" — a lower
  // step must mean a narrower track (more per row) and a higher step a wider one.
  it('is monotonically increasing in the step, so the slider direction is meaningful', () => {
    const widths = [0, 1, 2, 3, 4, 5].map(s => cardTrackPx(s, 112));
    for (let i = 1; i < widths.length; i++) expect(widths[i]).toBeGreaterThan(widths[i - 1]);
  });

  it('spans a range wide enough to be worth a slider', () => {
    expect(cardTrackPx(CARD_SIZE_MIN, 112)).toBeLessThan(80);
    expect(cardTrackPx(CARD_SIZE_MAX, 112)).toBeGreaterThan(200);
  });

  it('clamps absolutely, so no base × step combination escapes a usable size', () => {
    expect(cardTrackPx(CARD_SIZE_MIN, 20)).toBe(TRACK_MIN_PX);
    expect(cardTrackPx(CARD_SIZE_MAX, 400)).toBe(TRACK_MAX_PX);
  });

  it('returns integral pixels', () => {
    for (let s = CARD_SIZE_MIN; s <= CARD_SIZE_MAX; s++) {
      expect(Number.isInteger(cardTrackPx(s, 112))).toBe(true);
    }
  });

  it('treats an unusable stored step as the default rather than throwing', () => {
    expect(cardTrackPx('nonsense', 112)).toBe(112);
  });
});

describe('cardGridTemplate', () => {
  // auto-fill must survive: it is what makes the grid reflow on window resize at ANY
  // density. Losing it (or swapping in auto-fit) is the regression that would make the
  // slider work while the resize behaviour quietly stopped.
  it('keeps auto-fill so the grid still reflows on resize at every density', () => {
    for (let s = CARD_SIZE_MIN; s <= CARD_SIZE_MAX; s++) {
      expect(cardGridTemplate(s, 112)).toContain('auto-fill');
      expect(cardGridTemplate(s, 112)).not.toContain('auto-fit');
    }
  });

  // Without min(…,100%) a 220px track in a 180px phone-width container overflows the
  // viewport — the whole grid scrolls sideways. This is the mobile form factor's fix.
  it('lets a single column collapse below the track width instead of overflowing a phone', () => {
    expect(cardGridTemplate(CARD_SIZE_DEFAULT, 220)).toBe(
      'repeat(auto-fill, minmax(min(220px, 100%), 1fr))',
    );
  });

  it('still stretches the last partial row via 1fr', () => {
    expect(cardGridTemplate(CARD_SIZE_DEFAULT, 112)).toContain('1fr');
  });

  it('carries the resolved track width through', () => {
    expect(cardGridTemplate(CARD_SIZE_MAX, 112)).toContain(`${cardTrackPx(CARD_SIZE_MAX, 112)}px`);
  });
});

describe('cardSizeLabel', () => {
  it('names the step', () => {
    expect(cardSizeLabel(CARD_SIZE_DEFAULT)).toBe('Default');
    expect(cardSizeLabel(CARD_SIZE_MIN)).toBe('Smallest');
    expect(cardSizeLabel(CARD_SIZE_MAX)).toBe('Largest');
  });

  // The container width is known only to the browser, so the readout must not assert one.
  it('does not claim a per-row count this module cannot know', () => {
    for (let s = CARD_SIZE_MIN; s <= CARD_SIZE_MAX; s++) {
      expect(cardSizeLabel(s)).not.toMatch(/\d/);
      expect(cardSizeLabel(s).toLowerCase()).not.toContain('per row');
    }
  });

  it('never returns undefined for a hostile stored value', () => {
    expect(cardSizeLabel('nonsense')).toBe('Default');
    expect(cardSizeLabel(99)).toBe('Largest');
  });
});

describe('contentMaxWidth', () => {
  const panel = (id: string, path: string, wide?: boolean): PanelDescriptor =>
    ({ id, system: 'muse', title: id, path, available: true, wide, component: () => null }) as PanelDescriptor;

  const panels = [
    panel('muse.library', '/muse/library', true),
    panel('muse.library.detail', '/muse/library/:id', true),
    panel('muse.taste', '/muse/taste'),
    panel('harmony.dashboard', '/harmony/dashboard'),
  ];

  it('lets a catalog page use the wide cap so an ultrawide is not a 1280px column', () => {
    expect(contentMaxWidth('/muse/library', panels)).toBe('var(--content-max-wide)');
  });

  it('matches a parameterised detail route as a route, not a prefix', () => {
    expect(contentMaxWidth('/muse/library/1225', panels)).toBe('var(--content-max-wide)');
  });

  // POL-03's reading measure must survive for every panel that did not opt in.
  it('leaves every non-opted-in panel on the standard cap', () => {
    expect(contentMaxWidth('/muse/taste', panels)).toBe('var(--content-max)');
    expect(contentMaxWidth('/harmony/dashboard', panels)).toBe('var(--content-max)');
  });

  it('falls back to the standard cap for an unrouted path', () => {
    expect(contentMaxWidth('/overview', panels)).toBe('var(--content-max)');
    expect(contentMaxWidth('/', panels)).toBe('var(--content-max)');
  });

  // A prefix test would widen this; matchPath must not.
  it('does not widen a different panel whose path merely starts with a wide one', () => {
    const withSibling = [...panels, panel('muse.library-archive', '/muse/library-archive')];
    expect(contentMaxWidth('/muse/library-archive', withSibling)).toBe('var(--content-max)');
  });

  it('is safe with no panels registered yet', () => {
    expect(contentMaxWidth('/muse/library', [])).toBe('var(--content-max)');
  });
});
