// S127 TGUI2 POL-07: the contextual toolbar (Linear pattern) that sits above every data table —
// a clean search field + neutral filter controls on the left, a live result count on the right.
// Replaces the loose rows of saturated filter "chips" (the colored-chip soup the design flags):
// nothing here is saturated at rest; the active state of a segment is a subtle neutral fill + a
// thin accent inset, never a rainbow. All geometry is in the `.h-toolbar / .h-input / .h-select /
// .h-seg*` classes (token-only, no raw px/hex in this file).
import type { ReactNode } from 'react';

/** The toolbar shell: left-aligned controls, an elastic spacer, then right-aligned content. */
export function Toolbar({ children, right }: { children: ReactNode; right?: ReactNode }) {
  return (
    <div className="h-toolbar">
      {children}
      {right && <><span className="h-toolbar-spacer" />{right}</>}
    </div>
  );
}

/** Neutral search input with a `/`-style affordance omitted (kept minimal); grows to fill. */
export function SearchInput({
  value,
  onChange,
  placeholder = 'Search…',
  ariaLabel,
  grow = true,
}: {
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  ariaLabel?: string;
  grow?: boolean;
}) {
  return (
    <input
      type="text"
      className="h-input"
      value={value}
      onChange={e => onChange(e.target.value)}
      placeholder={placeholder}
      aria-label={ariaLabel ?? placeholder}
      style={grow ? { flex: '1 1 220px', minWidth: 0 } : undefined}
    />
  );
}

export interface SegmentOption<V extends string> {
  id: V;
  label: string;
}

/** Neutral segmented control (Cloudflare/Linear) — replaces a row of colored toggle pills. */
export function SegmentedControl<V extends string>({
  options,
  value,
  onChange,
  ariaLabel,
}: {
  options: SegmentOption<V>[];
  value: V;
  onChange: (v: V) => void;
  ariaLabel?: string;
}) {
  return (
    <div className="h-seg" role="group" aria-label={ariaLabel}>
      {options.map(o => (
        <button
          key={o.id}
          type="button"
          className="h-seg-btn"
          data-active={value === o.id}
          aria-pressed={value === o.id}
          onClick={() => onChange(o.id)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

/** Neutral labelled filter dropdown. `null`/'' option value clears the filter. */
export function FilterSelect({
  label,
  value,
  onChange,
  options,
  allLabel = 'All',
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  options: Array<{ value: string; label: string }>;
  allLabel?: string;
}) {
  return (
    <label style={{ display: 'inline-flex', alignItems: 'center', gap: 'var(--space-2)' }}>
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', textTransform: 'uppercase', letterSpacing: 'var(--ls-label)', color: 'var(--text-400)' }}>
        {label}
      </span>
      <select className="h-select" value={value} onChange={e => onChange(e.target.value)} aria-label={label}>
        <option value="">{allLabel}</option>
        {options.map(o => <option key={o.value} value={o.value}>{o.label}</option>)}
      </select>
    </label>
  );
}

/** The right-aligned live result count for the toolbar. */
export function ResultCount({ count, noun = 'result' }: { count: number; noun?: string }) {
  return (
    <span className="h-dtable-count">
      {count} {noun}{count === 1 ? '' : 's'}
    </span>
  );
}
