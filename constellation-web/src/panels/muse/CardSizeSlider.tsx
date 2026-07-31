// MGUI-18: the media-card size control, shared by every Muse catalog grid.
//
// The operator asked for "a slider to increase or decrease the media cards grid so i can fit
// more or less items on the page". Three deliberate choices:
//
//  1. A NATIVE `<input type="range">`, not a custom widget. It is keyboard-operable (arrows,
//     Home/End, Page Up/Down), announces its label and value to a screen reader, and honours
//     the platform's own pointer/touch handling — all for free, none of which a div-with-
//     mousemove reimplements correctly.
//  2. ONE preference for all catalog grids (`prefs['museCardSize']`), not one per page. Each
//     panel keeps its own base track width, so a single step means "the same relative size"
//     on the poster wall and on the wider provider cards.
//  3. The readout names the STEP ("Default", "Larger"), never a per-row count. How many cards
//     actually fit is a function of the container's real width, which only the browser knows;
//     printing "≈8 per row" would be a claim this component cannot support and would be wrong
//     the instant the window is resized or the rail collapses.
//
// The slider does NOT drive the reflow. `auto-fill` does. All this changes is the `minmax()`
// track floor, so the grid keeps re-flowing on window resize at every density (that property
// is pinned by a test in lib/catalogLayout.test.ts).
import { useCallback, useId, useState } from 'react';
import { getAggregationClient } from '../../lib/aggregationClient';
import {
  CARD_SIZE_MAX,
  CARD_SIZE_MIN,
  cardSizeLabel,
  clampCardSize,
  type CardSizeStep,
} from '../../lib/catalogLayout';

/** Reads the persisted card size and writes it back on change.
 *
 *  The stored value goes through `clampCardSize`, so a corrupt/absent/older-schema entry
 *  yields the default rather than an out-of-range index — a preference is a convenience and
 *  must never be able to break the grid it configures. */
export function useCardSize(): [CardSizeStep, (next: number) => void] {
  const [step, setStep] = useState<CardSizeStep>(() =>
    clampCardSize(getAggregationClient().prefs.get<number>('museCardSize')),
  );
  const set = useCallback((next: number) => {
    const clamped = clampCardSize(next);
    setStep(clamped);
    // The prefs seam already no-ops when storage is unavailable (private mode / quota), so a
    // failed write costs the operator the persistence, never the setting itself this session.
    getAggregationClient().prefs.set('museCardSize', clamped);
  }, []);
  return [step, set];
}

export function CardSizeSlider({
  value,
  onChange,
}: {
  value: CardSizeStep;
  onChange: (next: number) => void;
}) {
  const id = useId();
  return (
    <div
      className="muse-card-size"
      style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', minWidth: 0 }}
    >
      <label
        htmlFor={id}
        style={{
          fontSize: 'var(--fs-2xs, 10px)',
          fontFamily: 'var(--font-mono)',
          textTransform: 'uppercase',
          letterSpacing: '0.04em',
          color: 'var(--text-400, var(--text-300))',
          whiteSpace: 'nowrap',
        }}
      >
        Card size
      </label>
      {/* The two end glyphs say which direction does what without needing a sentence — small
          square on the left (more, smaller cards), large square on the right (fewer, bigger).
          They are decorative: the real explanation is the input's own accessible name. */}
      <span aria-hidden style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-400, var(--text-300))' }}>▪</span>
      <input
        id={id}
        type="range"
        min={CARD_SIZE_MIN}
        max={CARD_SIZE_MAX}
        step={1}
        value={value}
        onChange={e => onChange(Number(e.target.value))}
        aria-label="Media card size — smaller fits more cards per row, larger fits fewer"
        aria-valuetext={cardSizeLabel(value)}
        style={{ width: 108, accentColor: 'var(--accent-primary)', cursor: 'pointer' }}
      />
      <span aria-hidden style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-400, var(--text-300))' }}>◼</span>
      {/* Live readout so a keyboard user gets the same feedback a dragging pointer does. */}
      <span
        aria-live="polite"
        style={{
          fontSize: 'var(--fs-2xs, 10px)',
          fontFamily: 'var(--font-mono)',
          color: 'var(--text-300)',
          minWidth: 56,
          whiteSpace: 'nowrap',
        }}
      >
        {cardSizeLabel(value)}
      </span>
    </div>
  );
}
