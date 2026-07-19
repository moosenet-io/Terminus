// CONST-17: ChartTooltip — value-leads-label rows, line-key swatches, brand chrome (§4.2/4.3).
// SECURITY: series/point labels can originate from untrusted upstream data (model names,
// run case_ids, etc.) — every label is inserted via `textContent` (through the `Label`
// sub-component below), never `dangerouslySetInnerHTML` or raw HTML interpolation. Plain
// JSX text children are already textContent-equivalent (React escapes them), but the
// explicit ref+textContent path here makes that guarantee grep-visible and immune to any
// future refactor that swaps in an HTML-accepting prop by mistake.
import { useEffect, useRef } from 'react';
import { getVizTheme } from './theme';

/** Renders `text` via `.textContent` only — the sanctioned way to show an untrusted label. */
function Label({ text, style }: { text: string; style?: React.CSSProperties }) {
  const ref = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    if (ref.current) ref.current.textContent = text;
  }, [text]);
  return <span ref={ref} style={style} />;
}

export interface ChartTooltipRow {
  key: string;
  label: string;
  value: string;
  color?: string;
}

interface ChartTooltipProps {
  title?: string;
  rows: ChartTooltipRow[];
}

export function ChartTooltip({ title, rows }: ChartTooltipProps) {
  const theme = getVizTheme();
  return (
    <div
      style={{
        background: theme.tooltipBg,
        border: `1px solid ${theme.tooltipBorder}`,
        borderRadius: 8,
        boxShadow: theme.tooltipShadow,
        padding: 'var(--space-2) var(--space-3)',
        fontFamily: theme.fontMono,
        fontSize: 12,
        minWidth: 120,
      }}
    >
      {title && (
        <Label
          text={title}
          style={{ display: 'block', color: 'var(--text-100)', fontWeight: 600, marginBottom: 4 }}
        />
      )}
      {rows.map(r => (
        <div key={r.key} style={{ display: 'flex', alignItems: 'center', gap: 6, justifyContent: 'space-between' }}>
          <span style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
            {r.color && (
              <span aria-hidden style={{ width: 8, height: 8, borderRadius: 2, background: r.color, flexShrink: 0 }} />
            )}
            <Label text={r.label} style={{ color: 'var(--text-muted)' }} />
          </span>
          {/* value-leads-label: value gets the emphasized ink */}
          <Label text={r.value} style={{ color: 'var(--text-100)', fontWeight: 500, fontVariantNumeric: 'tabular-nums' }} />
        </div>
      ))}
    </div>
  );
}
