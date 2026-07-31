// MGUI-18: the sizing rules behind the Muse catalog surfaces (Library poster wall, Discover,
// Search & request results, the provider catalog, the media-detail bench).
//
// The operator's complaint was two separate faults, and this module holds the decision logic
// for both so each one is a pure function with a test rather than an inline expression:
//
//   1. "the card area does not resize with the window"
//      Two causes. (a) Every Muse panel passed `ChartCard` a FIXED pixel body height
//      (`height={720}` / `560` / `620`), so a 1440p-tall portrait monitor got the same 720px
//      box as a 768px laptop — a void below on one, a cramped scroller on the other. (b) The
//      shell caps its canvas at `--content-max` (1280px), so an ultrawide got a 1280px column
//      and empty desk either side. `fluidBodyHeight()` fixes (a); `contentMaxWidth()` fixes (b)
//      for the panels that opt in.
//
//   2. "a slider to increase or decrease the media cards grid"
//      `cardGridTemplate()` turns a discrete slider step into the grid's `minmax()` track.
//
// EVERYTHING HERE IS CSS, NOT JS MEASUREMENT. `clamp()` + `auto-fill` re-resolve on every
// window resize with no `resize` listener, no re-render, and no layout thrash — which also
// means none of it can report a stale size after a resize the way a JS-measured value can.
// It also means this module can never claim how many cards actually fit per row: only the
// browser knows the container's real width. Nothing here renders such a claim.

import { matchPath } from 'react-router-dom';
import type { PanelDescriptor } from './moduleRegistry';

// ── 1. Fluid panel body height ───────────────────────────────────────────────

/** A fluid body height, in the three numbers a `clamp()` needs.
 *
 *  `reserve` is the vertical space the panel body does NOT get: the global bar (52px), the
 *  ChartCard's own padding + header row, and anything stacked above the card on that page.
 *  It is deliberately approximate — `min`/`max` are the guarantees, `reserve` only decides
 *  where between them a given viewport lands. */
export interface FluidBodySpec {
  /** Floor. Below this the surface stops being readable, so it scrolls the page instead. */
  min: number;
  /** Ceiling. Stops an ultrawide/portrait viewport from stretching one card absurdly tall. */
  max: number;
  /** Viewport height consumed by chrome above/around this body. */
  reserve: number;
}

/**
 * A CSS length that tracks the viewport between `min` and `max`.
 *
 * `dvh` (dynamic viewport height), not `vh`: on mobile browsers `vh` is pinned to the
 * LARGEST viewport (address bar hidden), so a `100vh - reserve` box overflows behind the
 * collapsed-state address bar on first paint. `dvh` follows the visible viewport.
 *
 * A `max` below `min` would emit a backwards clamp (CSS resolves those to the min, silently),
 * so it is normalised up to `min` here where a test can pin the behaviour. A negative
 * `reserve` is normalised to 0 rather than secretly ADDING height to the viewport.
 */
export function fluidBodyHeight(spec: FluidBodySpec): string {
  const min = Math.max(0, Math.round(spec.min));
  const max = Math.max(min, Math.round(spec.max));
  const reserve = Math.max(0, Math.round(spec.reserve));
  return `clamp(${min}px, calc(100dvh - ${reserve}px), ${max}px)`;
}

// ── 2. Media-card density ────────────────────────────────────────────────────

/** The slider's discrete positions. A range input of 6 stops, not a continuous px value:
 *  a continuous track width lets an operator land on a size that fits 3.02 columns and wastes
 *  most of a row, and it makes the stored preference impossible to label. */
export const CARD_SIZE_STEPS = ['xs', 'sm', 'md', 'lg', 'xl', 'xxl'] as const;
export type CardSizeStep = number; // index into CARD_SIZE_STEPS

export const CARD_SIZE_MIN = 0;
export const CARD_SIZE_MAX = CARD_SIZE_STEPS.length - 1;

/** The default is the index whose multiplier is exactly 1.0, so an operator who never touches
 *  the slider sees the EXACT grid that shipped before this item — no jarring reflow on upgrade.
 *  This is asserted in the tests against `CARD_SIZE_SCALE`, not just declared here. */
export const CARD_SIZE_DEFAULT = 2;

/** Multiplier applied to a panel's base track width. Roughly geometric (~1.25×/step) so each
 *  notch is a visible but not violent change, and index 2 is identity. */
export const CARD_SIZE_SCALE = [0.6, 0.78, 1, 1.28, 1.62, 2] as const;

/** Human-readable step names, for the slider's readout. */
export const CARD_SIZE_LABELS = ['Smallest', 'Small', 'Default', 'Large', 'Larger', 'Largest'] as const;

/** Absolute bounds on the resolved track, independent of base and step. Below ~56px a poster
 *  is not identifiable; above ~420px even the widest card (the provider catalog's 220px base
 *  at the top step) stops being a grid and becomes a list. */
export const TRACK_MIN_PX = 56;
export const TRACK_MAX_PX = 420;

/**
 * Coerce anything — a `localStorage` round-trip, a range input's string value, a stale
 * preference written by an older build with a different step count — into a valid step.
 *
 * Returns `CARD_SIZE_DEFAULT` for anything non-integral or unparsable, and clamps (rather
 * than rejecting) an out-of-range integer, so shrinking the scale in a future build degrades
 * a stored `5` to the new top step instead of silently resetting an operator's preference.
 */
export function clampCardSize(value: unknown): CardSizeStep {
  const n = typeof value === 'string' ? Number(value) : value;
  if (typeof n !== 'number' || !Number.isFinite(n)) return CARD_SIZE_DEFAULT;
  const i = Math.round(n);
  if (i < CARD_SIZE_MIN) return CARD_SIZE_MIN;
  if (i > CARD_SIZE_MAX) return CARD_SIZE_MAX;
  return i;
}

/**
 * Each catalog grid's base track width — the `minmax()` floor it shipped with, and therefore
 * the width the DEFAULT slider step must reproduce to the pixel.
 *
 * These live here, not as private constants in each panel, so the "an operator who never
 * touches the slider sees no change" guarantee is checkable: the test asserts against the
 * SAME values the panels render from, rather than against numbers transcribed into the test
 * that could silently drift apart from the panels.
 */
export const CATALOG_TRACK_BASE = {
  /** Library poster wall (LibraryPanel). */
  poster: 112,
  /** Discover's trending posters (DiscoverPanel). */
  discover: 120,
  /** Search & request result tiles (RequestPanel) — wider, they carry a request button. */
  searchResult: 132,
  /** The provider catalog's text cards (RequestPanel) — widest, they carry prose. */
  provider: 220,
} as const;

/** The `minmax()` track width for a panel whose shipped default track was `basePx`.
 *  Each panel keeps its own base (posters 112, discover 120, results 132, provider cards 220)
 *  so one slider position means "the same relative size" everywhere, not one literal width
 *  imposed on surfaces with different content. */
export function cardTrackPx(step: unknown, basePx: number): number {
  const scaled = Math.round(basePx * CARD_SIZE_SCALE[clampCardSize(step)]);
  return Math.min(TRACK_MAX_PX, Math.max(TRACK_MIN_PX, scaled));
}

/**
 * The full `grid-template-columns` value.
 *
 * `min(<track>px, 100%)` inside the `minmax()` is load-bearing, not decoration: a bare
 * `minmax(220px, 1fr)` in a 180px-wide container produces a 220px column that OVERFLOWS —
 * the classic auto-fill mobile bug. `min(…, 100%)` lets the single column collapse to the
 * container instead, which is what makes a large card size still usable on a phone.
 *
 * `auto-fill` (not `auto-fit`) is kept from the original: `auto-fit` collapses empty tracks
 * and stretches a lone result to the full row width, which looks like a bug when a filter
 * matches one title.
 */
export function cardGridTemplate(step: unknown, basePx: number): string {
  return `repeat(auto-fill, minmax(min(${cardTrackPx(step, basePx)}px, 100%), 1fr))`;
}

/** The slider's readout. Names the step; deliberately does NOT name a per-row count, which
 *  depends on the container width only the browser knows (see the module header). */
export function cardSizeLabel(step: unknown): string {
  return CARD_SIZE_LABELS[clampCardSize(step)];
}

// ── 3. Canvas width for wide (catalog) panels ────────────────────────────────

/**
 * Which content-width cap the shell canvas should use for the current route.
 *
 * POL-03 capped the canvas at `--content-max` (~1280px) and centred it — right for a column
 * of text and charts, wrong for a 1892-tile poster wall, which is the surface the operator was
 * looking at on an ultrawide when they said the card area "does not resize with the window".
 * So the cap is per-panel (`PanelDescriptor.wide`) rather than raised globally: every existing
 * panel keeps POL-03's reading measure untouched.
 *
 * `matchPath` (not `startsWith`) so a parameterised route like `/muse/library/:id` is matched
 * as a route, and so `/muse/library-archive` could never be mistaken for `/muse/library`.
 * Order matters: the first `wide` panel that matches wins, and a non-wide exact match does not
 * veto a later wide one — but no two registered panels share a path, so at most one matches.
 */
export function contentMaxWidth(pathname: string, panels: readonly PanelDescriptor[]): string {
  const hit = panels.find(p => matchPath({ path: p.path, end: true }, pathname) !== null);
  return hit?.wide ? 'var(--content-max-wide)' : 'var(--content-max)';
}
