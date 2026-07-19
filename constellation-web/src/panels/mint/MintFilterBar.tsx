// CONST-23: the ONE global filter row that scopes every MINT section (§7.1). Filters never
// live inside a ChartCard (§4.3) — this renders once, above the sticky section nav, and every
// section below reads the same `MintFilters` object. Model multi-select is capped at 4 (drives
// emphasis/series assignment everywhere per §7.1); selecting a 5th is a no-op with an inline
// note rather than a silent replace or a toast (no toast infra needed for a same-page control).
import { Badge } from '../../components/Badge';
import type { MintFilters } from '../../hooks/useMint';
import { MINT_MODEL_SELECT_CAP } from './mintFilters';
import { MINT_MODEL_CATALOG } from '../../lib/aggregationClient';

const selectStyle: React.CSSProperties = {
  background: 'var(--bg-elevated)',
  border: '1px solid var(--border)',
  borderRadius: 'var(--radius-sm)',
  color: 'var(--text-body)',
  padding: '5px 8px',
  fontSize: 'var(--fs-sm)',
  fontFamily: 'var(--font-sans)',
  outline: 'none',
};

const labelStyle: React.CSSProperties = {
  fontFamily: 'var(--font-mono)',
  fontSize: 'var(--fs-mono-sm)',
  color: 'var(--text-400)',
  textTransform: 'uppercase',
  letterSpacing: 'var(--ls-label)',
  marginRight: 6,
};

interface MintFilterBarProps {
  filters: MintFilters;
  onChange: (next: MintFilters) => void;
  epochOptions: { value: string; label: string }[];
}

export function MintFilterBar({ filters, onChange, epochOptions }: MintFilterBarProps) {
  const toggleModel = (model: string) => {
    const has = filters.models.includes(model);
    if (has) {
      onChange({ ...filters, models: filters.models.filter(m => m !== model) });
      return;
    }
    if (filters.models.length >= MINT_MODEL_SELECT_CAP) return; // at cap — no-op, see header note
    onChange({ ...filters, models: [...filters.models, model] });
  };

  return (
    <div
      style={{
        display: 'flex',
        flexWrap: 'wrap',
        alignItems: 'center',
        gap: 'var(--space-4)',
        padding: 'var(--space-3) var(--space-4)',
        background: 'var(--bg-panel)',
        borderBottom: '1px solid var(--border)',
        position: 'sticky',
        top: 0,
        zIndex: 5,
      }}
    >
      <div>
        <span style={labelStyle}>Epoch</span>
        <select
          style={selectStyle}
          value={filters.epoch}
          onChange={e => onChange({ ...filters, epoch: e.target.value })}
        >
          {epochOptions.map(o => (
            <option key={o.value} value={o.value}>{o.label}</option>
          ))}
        </select>
      </div>

      <div>
        <span style={labelStyle}>Task category</span>
        <select
          style={selectStyle}
          value={filters.taskCategory}
          onChange={e => onChange({ ...filters, taskCategory: e.target.value as MintFilters['taskCategory'] })}
        >
          <option value="all">All</option>
          <option value="blitz">Blitz</option>
          <option value="multi_file">Multi-file</option>
          <option value="deep">Deep</option>
        </select>
      </div>

      <div>
        <span style={labelStyle}>Backend</span>
        <select
          style={selectStyle}
          value={filters.backendTag}
          onChange={e => onChange({ ...filters, backendTag: e.target.value as MintFilters['backendTag'] })}
        >
          <option value="all">All</option>
          <option value="gpu">GPU</option>
          <option value="cpu">CPU</option>
        </select>
      </div>

      <div style={{ display: 'flex', alignItems: 'center', gap: 6, flexWrap: 'wrap' }}>
        <span style={labelStyle}>
          Models ({filters.models.length}/{MINT_MODEL_SELECT_CAP})
        </span>
        {MINT_MODEL_CATALOG.map(model => {
          const active = filters.models.includes(model);
          const atCap = !active && filters.models.length >= MINT_MODEL_SELECT_CAP;
          return (
            <button
              key={model}
              type="button"
              onClick={() => toggleModel(model)}
              disabled={atCap}
              aria-pressed={active}
              style={{
                border: 'none',
                background: 'transparent',
                padding: 0,
                cursor: atCap ? 'not-allowed' : 'pointer',
                opacity: atCap ? 0.4 : 1,
              }}
            >
              <Badge tone={active ? 'violet' : 'neutral'} mono>{model}</Badge>
            </button>
          );
        })}
      </div>
    </div>
  );
}
