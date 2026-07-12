// WIRE-07: SVG animated connection path between worker and engine nodes
interface Props {
  x1: number;
  y1: number;
  x2: number;
  y2: number;
  active: boolean;
}

export function FlowConnection({ x1, y1, x2, y2, active }: Props) {
  const color = active ? '#22d3ee' : 'var(--border-subtle)';
  const d = `M ${x1} ${y1} C ${x1 + 60} ${y1}, ${x2 - 60} ${y2}, ${x2} ${y2}`;
  return (
    <path
      d={d}
      stroke={color}
      strokeWidth={active ? 2 : 1}
      fill="none"
      strokeDasharray={active ? '0' : '4 4'}
      style={{ opacity: active ? 1 : 0.4, transition: 'stroke 0.3s' }}
    />
  );
}
