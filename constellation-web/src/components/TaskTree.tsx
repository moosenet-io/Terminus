// Organic task tree — SVG trunk + bezier branches + fruit/leaf nodes.
// Replaces the old absolute-positioned grid layout.
import { useMemo, useRef, useEffect, useState } from 'react';
import type { TreeStage } from '../types/tree';

// ─── Types (re-exported — Dashboard.tsx imports TaskItem + TaskStatus) ────────

export type TaskStatus = 'pending' | 'active' | 'complete' | 'done' | 'failed';

export interface TaskItem {
  id: string;
  label: string;
  status: TaskStatus;
  parentId?: string | null;
  isSpec?: boolean;
  specTitle?: string;
  itemId?: string;
  stages?: TreeStage[];
  held?: boolean;
  triageStep?: string;
  elapsed_secs?: number;
}

interface TaskTreeProps {
  items: TaskItem[];
  width?: number;
  height?: number; // minimum; actual height is computed from spec count
}

// ─── Constants ────────────────────────────────────────────────────────────────

const C = {
  trunk:        '#7c3e0a',
  trunkSheen:   '#b45309',
  branch:       '#a16207',
  branchActive: '#d97706',
  done:         '#10B981',
  doneBorder:   '#059669',
  active:       '#F59E0B',
  activeBorder: '#d97706',
  failed:       '#EF4444',
  failedBorder: '#b91c1c',
  held:         '#F59E0B',
  pending:      '#4b5563',
  pendingFill:  '#1f2937',
  specLabel:    '#9ca3af',
  specSub:      '#6b7280',
  root:         '#6b3f06',
};

// ─── Bezier helpers ───────────────────────────────────────────────────────────

function bezierPt(
  p0: [number, number], p1: [number, number],
  p2: [number, number], p3: [number, number],
  t: number,
): [number, number] {
  const u = 1 - t;
  return [
    u * u * u * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t * t * t * p3[0],
    u * u * u * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t * t * t * p3[1],
  ];
}

function fmtElapsed(secs: number): string {
  if (!secs || secs <= 0) return '';
  return secs >= 60 ? `${Math.floor(secs / 60)}m` : `${secs}s`;
}

function completionRatio(children: TaskItem[]): number {
  if (!children.length) return 0;
  const done = children.filter(c => c.status === 'complete' || c.status === 'done').length;
  return done / children.length;
}

// ─── Fruit / Leaf Node ────────────────────────────────────────────────────────

function FruitNode({ item, x, y }: { item: TaskItem; x: number; y: number }) {
  const prevStatus = useRef<TaskStatus>(item.status);
  const [popScale, setPopScale] = useState(1);

  useEffect(() => {
    if (prevStatus.current === item.status) return;
    prevStatus.current = item.status;
    setPopScale(1.45);
    const id = setTimeout(() => setPopScale(1), 350);
    return () => clearTimeout(id);
  }, [item.status]);

  const isDone    = item.status === 'complete' || item.status === 'done';
  const isActive  = item.status === 'active';
  const isFailed  = item.status === 'failed';
  const isPending = item.status === 'pending';
  const isHeld    = !!item.held;

  const elapsed = fmtElapsed(item.elapsed_secs ?? 0);
  const tooltipParts: string[] = [item.itemId ?? item.id, item.label];
  if (isHeld)              tooltipParts.push('HELD');
  if (elapsed && isActive) tooltipParts.push(elapsed);
  if (isFailed)            tooltipParts.push('FAILED');
  const tooltip = tooltipParts.filter(Boolean).join(' · ');

  if (isPending) {
    // Small leaf ellipse — backlog items don't warrant a full circle
    return (
      <ellipse cx={x} cy={y} rx={8} ry={5}
        fill={C.pendingFill} fillOpacity={0.85}
        stroke={C.pending} strokeWidth={1}
      >
        <title>{tooltip}</title>
      </ellipse>
    );
  }

  const fill   = isHeld ? C.held : isDone ? C.done : isActive ? C.active : C.failed;
  const border = isDone ? C.doneBorder : isActive ? C.activeBorder : C.failedBorder;
  const r      = isActive ? 11 : isDone ? 9 : 8;
  const icon   = isDone ? '✓' : isFailed ? '✗' : isActive ? '◉' : '';

  // Scale-from-center in SVG: translate to (x,y), scale, everything is relative to origin
  return (
    <g transform={`translate(${x},${y}) scale(${popScale})`} style={{ cursor: 'default' }}>
      <title>{tooltip}</title>

      {/* Pulse halo for active nodes */}
      {isActive && (
        <circle r={r + 5} fill={C.active} fillOpacity={0.3}>
          <animate attributeName="r"
            values={`${r + 4};${r + 10};${r + 4}`}
            dur="2s" repeatCount="indefinite" />
          <animate attributeName="fill-opacity"
            values="0.28;0;0.28"
            dur="2s" repeatCount="indefinite" />
        </circle>
      )}

      {/* Amber halo for held nodes */}
      {isHeld && !isActive && (
        <circle r={r + 4} fill={C.held} fillOpacity={0.2}>
          <animate attributeName="fill-opacity"
            values="0.2;0.05;0.2"
            dur="2.5s" repeatCount="indefinite" />
        </circle>
      )}

      {/* Main fruit circle */}
      <circle r={r} fill={fill} stroke={border} strokeWidth={isDone ? 2 : 1.5} />

      {/* Status icon */}
      {icon && (
        <text textAnchor="middle" dominantBaseline="central"
          fontSize={r <= 9 ? 8 : 9} fill="white" fontWeight="bold" pointerEvents="none">
          {icon}
        </text>
      )}
    </g>
  );
}

// ─── Main Component ───────────────────────────────────────────────────────────

export function TaskTree({ items, width = 800 }: TaskTreeProps) {
  // Group into spec→children, sort most-complete to top of tree (lowest Y)
  const specs = useMemo(() => {
    const specNodes = items.filter(i => i.isSpec);
    const childMap = new Map<string, TaskItem[]>();
    for (const item of items) {
      if (item.parentId) {
        const arr = childMap.get(item.parentId) ?? [];
        arr.push(item);
        childMap.set(item.parentId, arr);
      }
    }
    return specNodes
      .map(s => ({ spec: s, children: childMap.get(s.id) ?? [] }))
      .sort((a, b) => completionRatio(b.children) - completionRatio(a.children));
  }, [items]);

  const N = specs.length;

  if (N === 0) {
    return (
      <div className="tree-unavailable">
        <span>Tree unavailable</span>
      </div>
    );
  }

  // Dynamic canvas sizing
  const height     = Math.max(420, Math.min(800, 340 + N * 68));
  const cx         = width / 2;
  const trunkBaseY = height - 28;
  const trunkTopY  = 52;
  const trunkMidY  = (trunkBaseY + trunkTopY) / 2;

  // Slight S-curve trunk for organic feel
  const trunkD = `M ${cx} ${trunkBaseY} C ${cx + 11} ${trunkBaseY - 65}, ${cx - 9} ${trunkMidY}, ${cx} ${trunkTopY}`;

  // Max branch length — never lets labels clip the viewport
  const maxBLen = Math.min(215, cx - 55);

  return (
    <div style={{ width, overflowX: 'hidden' }}>
      <svg width={width} height={height} viewBox={`0 0 ${width} ${height}`} style={{ display: 'block' }}>

        {/* ── Trunk ── */}
        <path d={trunkD} fill="none" stroke={C.trunk} strokeWidth={8} strokeLinecap="round" />
        {/* Highlight stripe */}
        <path d={trunkD} fill="none" stroke={C.trunkSheen} strokeWidth={2} strokeLinecap="round" opacity={0.3} />

        {/* ── Branches + nodes ── */}
        {specs.map(({ spec, children }, i) => {
          // t=0 → top of trunk (most complete), t→1 → lower
          const t = N === 1 ? 0.4 : (i / (N - 1)) * 0.82 + 0.06;
          const attachY = trunkTopY + (trunkBaseY - trunkTopY) * t;

          const side: 1 | -1 = i % 2 === 0 ? 1 : -1;

          // Lower branches a bit longer (they have more visual weight at bottom)
          const bLen = maxBLen * (0.72 + 0.28 * t);
          const endX  = cx + side * bLen;
          const endY  = attachY - 18 + (i % 3) * 7; // subtle Y variation per branch

          // Bezier control points — first pulls away from trunk, second curls back
          const cp1X = cx + side * bLen * 0.28;
          const cp1Y = attachY - 12;
          const cp2X = endX - side * bLen * 0.22;
          const cp2Y = endY + 12;

          const ratio     = completionRatio(children);
          const hasActive = children.some(c => c.status === 'active');
          const branchColor = hasActive ? C.branchActive : C.branch;
          const branchW     = hasActive ? 3.5 : ratio === 1 ? 1.5 : 2.5;
          const branchOp    = ratio === 1 ? 0.5 : 1;

          const branchD = `M ${cx} ${attachY} C ${cp1X} ${cp1Y}, ${cp2X} ${cp2Y}, ${endX} ${endY}`;

          const labelX      = side === 1 ? endX + 7 : endX - 7;
          const labelAnchor = side === 1 ? 'start' : 'end';

          return (
            <g key={spec.id}>
              {/* Branch */}
              <path
                d={branchD}
                fill="none"
                stroke={branchColor}
                strokeWidth={branchW}
                strokeLinecap="round"
                opacity={branchOp}
              />

              {/* Spec ID */}
              <text
                x={labelX} y={endY - 8}
                textAnchor={labelAnchor}
                fontSize={10}
                fontFamily="monospace"
                fill={C.specLabel}
                letterSpacing="0.04em"
              >
                {spec.id}
              </text>

              {/* Spec subtitle (truncated) */}
              {spec.specTitle && (
                <text
                  x={labelX} y={endY + 5}
                  textAnchor={labelAnchor}
                  fontSize={9}
                  fontFamily="sans-serif"
                  fill={C.specSub}
                >
                  {spec.specTitle.slice(0, 20)}
                </text>
              )}

              {/* Fruit nodes placed along bezier curve */}
              {children.map((child, j) => {
                const M  = children.length;
                const ft = M === 1 ? 0.62 : (j + 1) / (M + 1);
                const [px, py] = bezierPt(
                  [cx, attachY], [cp1X, cp1Y], [cp2X, cp2Y], [endX, endY], ft,
                );
                return <FruitNode key={child.id} item={child} x={px} y={py} />;
              })}
            </g>
          );
        })}

        {/* ── Surface roots (aesthetic) ── */}
        <path
          d={`M ${cx - 38} ${trunkBaseY} Q ${cx - 18} ${trunkBaseY + 14} ${cx} ${trunkBaseY}`}
          fill="none" stroke={C.root} strokeWidth={4} strokeLinecap="round" opacity={0.45}
        />
        <path
          d={`M ${cx + 38} ${trunkBaseY} Q ${cx + 18} ${trunkBaseY + 14} ${cx} ${trunkBaseY}`}
          fill="none" stroke={C.root} strokeWidth={4} strokeLinecap="round" opacity={0.45}
        />

        {/* ── All-done crown ── */}
        {specs.length > 0 && specs.every(s => completionRatio(s.children) === 1) && (
          <g>
            <circle cx={cx} cy={trunkTopY - 18} r={15} fill={C.done} opacity={0.9} />
            <text x={cx} y={trunkTopY - 18} textAnchor="middle" dominantBaseline="central" fontSize={13}>
              🌳
            </text>
          </g>
        )}
      </svg>
    </div>
  );
}
