// CONST-17: empty state for ChartCard — centered muted message + a one-line data-provenance
// hint (§2.6/§4.3), never a blank box.
interface ChartEmptyProps {
  height: number;
  message: string;
  hint?: string;
}

export function ChartEmpty({ height, message, hint }: ChartEmptyProps) {
  return (
    <div
      style={{
        height,
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        justifyContent: 'center',
        gap: 'var(--space-1)',
        color: 'var(--text-muted)',
        textAlign: 'center',
        padding: 'var(--space-3)',
      }}
    >
      <div style={{ fontSize: 'var(--fs-sm)' }}>{message}</div>
      {hint && (
        <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-faint)' }}>{hint}</div>
      )}
    </div>
  );
}
