// LGUI-09 (§3.4/§5): TraitSlider — one horizontal row for a single `TraitVector` axis
// (flair/spontaneity/humor/focus). Renders three things over one 0..1 rail, per spec:
//   - the soft-bound rails (0.15/0.85, §0.1.1) as two inset guide ticks
//   - the shared BASE marker (neutral) and the per-user MODIFIER delta (a bracket from base to
//     effective, labeled with its signed value)
//   - the clamped EFFECTIVE value as the violet fill (`effective = clamp(base + modifier)`)
// The actual editable control is a native `<input type="range">` (native for a11y: keyboard
// arrows, screen-reader value announcement) laid UNDER the visual rail, driving whichever of
// base/modifier the panel is currently editing (`editKind`) — dragging it moves `editValue`
// only; `PersonaPanel` recomputes `effective` from the single `useLuminaPersona` state so this
// component never disagrees with the radar (that comparison lives one level up).
import { useId } from 'react';
import type { LuminaPersonaBounds } from '../../types/lumina';

interface TraitSliderProps {
  label: string;
  base: number;
  modifier: number;
  effective: number;
  bounds: LuminaPersonaBounds;
  /** Which of base/modifier the draggable handle currently edits — 'base' for a direct admin
   *  edit of the shared default, 'modifier' for an admin-on-behalf per-user delta (§3.4 v1). */
  editKind: 'base' | 'modifier';
  /** The live (possibly unsaved) value of whichever field `editKind` names — NOT necessarily
   *  equal to `base`/`modifier` above once the operator has dragged but not yet saved. */
  editValue: number;
  onChange: (next: number) => void;
  disabled?: boolean;
}

const TRACK_MIN = 0;
const TRACK_MAX = 1;

function pct(v: number): number {
  return ((Math.min(TRACK_MAX, Math.max(TRACK_MIN, v)) - TRACK_MIN) / (TRACK_MAX - TRACK_MIN)) * 100;
}

export function TraitSlider({
  label, base, modifier, effective, bounds, editKind, editValue, onChange, disabled = false,
}: TraitSliderProps) {
  const id = useId();
  const deltaSign = modifier > 0 ? '+' : modifier < 0 ? '' : '±';

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'baseline' }}>
        <label htmlFor={id} style={{ fontSize: 'var(--fs-sm)', fontWeight: 600, color: 'var(--text-100)', textTransform: 'capitalize' }}>
          {label}
        </label>
        <span style={{ fontSize: 'var(--fs-xs)', fontFamily: 'var(--font-mono)', color: 'var(--text-muted)' }}>
          base {base.toFixed(2)} · mod {deltaSign}{modifier.toFixed(2)} · eff{' '}
          <strong style={{ color: 'var(--violet-400)' }}>{effective.toFixed(2)}</strong>
        </span>
      </div>

      <div style={{ position: 'relative', height: 28 }}>
        {/* Rail chrome: full-width track + soft-bound guide ticks + violet effective fill. */}
        <div
          aria-hidden
          style={{
            position: 'absolute', top: 12, left: 0, right: 0, height: 4,
            borderRadius: 'var(--radius-sm)', background: 'var(--border-subtle)', overflow: 'hidden',
          }}
        >
          <div
            style={{
              position: 'absolute', top: 0, bottom: 0, left: 0, width: `${pct(effective)}%`,
              background: 'var(--violet-400)', transition: 'width var(--dur-base, 160ms) var(--ease-out, ease)',
            }}
          />
        </div>

        {/* Soft-bound guides (0.15 / 0.85). */}
        {[bounds.min, bounds.max].map(b => (
          <div
            key={b}
            aria-hidden
            title={`soft bound ${b.toFixed(2)}`}
            style={{
              position: 'absolute', top: 8, left: `${pct(b)}%`, width: 2, height: 12,
              background: 'var(--text-muted)', opacity: 0.5, transform: 'translateX(-1px)',
            }}
          />
        ))}

        {/* Base marker (shared default). */}
        <div
          aria-hidden
          title={`base ${base.toFixed(2)}`}
          style={{
            position: 'absolute', top: 6, left: `${pct(base)}%`, width: 2, height: 16,
            background: 'var(--text-body)', transform: 'translateX(-1px)',
          }}
        />

        {/* Modifier delta bracket: base -> effective. Zero-width (no visible bracket) when the
            modifier is exactly 0 — nothing to show, avoids a stray dot artifact. */}
        {base !== effective && (
          <div
            aria-hidden
            style={{
              position: 'absolute', top: 13, height: 2,
              left: `${Math.min(pct(base), pct(effective))}%`,
              width: `${Math.abs(pct(effective) - pct(base))}%`,
              background: 'var(--flux-amber)', opacity: 0.7,
            }}
          />
        )}

        {/* The real, native, keyboard/screen-reader-accessible control — visually transparent
            track/thumb (browser chrome hidden via the .h-trait-slider class in globals-adjacent
            CSS is NOT assumed here; inline styles keep this self-contained) but still focusable
            and announces label + value normally. */}
        <input
          id={id}
          type="range"
          min={bounds.min}
          max={bounds.max}
          step={0.01}
          value={editValue}
          disabled={disabled}
          onChange={e => onChange(Number(e.target.value))}
          aria-label={`${label} ${editKind}`}
          aria-valuetext={`${editKind} ${editValue.toFixed(2)}, effective ${effective.toFixed(2)}`}
          style={{
            position: 'absolute', inset: 0, width: '100%', height: '100%',
            margin: 0, opacity: disabled ? 0.4 : 0.001, cursor: disabled ? 'not-allowed' : 'pointer',
          }}
        />
      </div>
    </div>
  );
}
