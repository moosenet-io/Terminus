// RMCP-13 (TERM-624): the assignment control shared by the tool-group and server pickers.
//
// A checkbox list rendered as chips rather than a `<select multiple>`: the operator has to see
// every option AND its current state at a glance (a native multi-select hides both), and each
// option carries a status note — "upstream down", "not yours to assign" — that a plain option
// list has nowhere to put.
//
// Options the session may not assign render DISABLED WITH THE REASON, never hidden. Hiding
// would make a delegated owner's view look like the namespace does not exist, which is both
// confusing and a worse security posture: the server refuses the write either way, and an
// operator who can see why is one who can ask for the delegation instead of filing a bug.
import { Badge } from '../../components/Badge';

export interface MultiSelectOption {
  value: string;
  label: string;
  /** Small note rendered under the label (tool count, owner, …). */
  detail?: string;
  /** Set when this option cannot be assigned by this session — rendered disabled WITH this
   *  reason shown. Cosmetic: the server refuses the write regardless. */
  disabledReason?: string;
  /** Shown as an "unavailable" chip: in scope but its upstream is currently down. Selectable —
   *  a temporarily-down upstream is not a reason to refuse a config change. */
  unavailable?: boolean;
}

export interface MultiSelectProps {
  legend: string;
  options: MultiSelectOption[];
  selected: string[];
  onChange: (next: string[]) => void;
  /** Renders every option non-interactive (viewer session, or a client this session cannot edit). */
  readOnly?: boolean;
  emptyMessage: string;
}

export function MultiSelect({ legend, options, selected, onChange, readOnly, emptyMessage }: MultiSelectProps) {
  const toggle = (value: string) => {
    onChange(selected.includes(value) ? selected.filter(v => v !== value) : [...selected, value]);
  };

  return (
    <fieldset style={{ border: 'none', padding: 0, margin: 0, minWidth: 0 }}>
      <legend
        style={{
          fontFamily: 'var(--font-mono)',
          fontSize: 'var(--fs-mono-sm)',
          letterSpacing: 'var(--ls-mono)',
          textTransform: 'uppercase',
          color: 'var(--text-500)',
          padding: 0,
          marginBottom: 'var(--space-2)',
        }}
      >
        {legend}
      </legend>

      {options.length === 0 ? (
        <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-400)' }}>{emptyMessage}</div>
      ) : (
        <div style={{ display: 'flex', flexWrap: 'wrap', gap: 'var(--space-2)' }}>
          {options.map(opt => {
            const checked = selected.includes(opt.value);
            const disabled = readOnly || Boolean(opt.disabledReason);
            return (
              <label
                key={opt.value}
                title={opt.disabledReason}
                style={{
                  display: 'flex',
                  alignItems: 'flex-start',
                  gap: 'var(--space-2)',
                  padding: 'var(--space-2) var(--space-3)',
                  borderRadius: 'var(--radius-sm)',
                  border: `var(--border-width) solid ${checked ? 'var(--line-accent)' : 'var(--border)'}`,
                  background: checked ? 'var(--accent-soft)' : 'var(--surface-chip)',
                  opacity: disabled ? 0.55 : 1,
                  cursor: disabled ? 'not-allowed' : 'pointer',
                  minWidth: 0,
                }}
              >
                <input
                  type="checkbox"
                  checked={checked}
                  disabled={disabled}
                  onChange={() => toggle(opt.value)}
                  style={{ marginTop: 'var(--space-1)', accentColor: 'var(--accent)' }}
                />
                <span style={{ minWidth: 0 }}>
                  <span style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)' }}>
                    <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-100)' }}>{opt.label}</span>
                    {opt.unavailable && <Badge tone="amber" dot>unavailable</Badge>}
                  </span>
                  {(opt.detail || opt.disabledReason) && (
                    <span
                      style={{
                        display: 'block',
                        fontFamily: 'var(--font-mono)',
                        fontSize: 'var(--fs-mono-sm)',
                        color: 'var(--text-400)',
                      }}
                    >
                      {opt.disabledReason ?? opt.detail}
                    </span>
                  )}
                </span>
              </label>
            );
          })}
        </div>
      )}
    </fieldset>
  );
}
