// SPOL-05: Agent Routing Diagram — compact horizontal pill layout.
// Shows provider pool availability per operating mode (behavior contract §2.3)
// and live activity state when engine is running.
import { useState } from 'react';
import type { RouteConnection } from '../hooks/useRoutingState';
import type { ModeId } from '../types/presets';

interface Props {
  connections: RouteConnection[];
  /** Current operating mode — determines which providers are in the pool */
  mode?: string;
}

const PROVIDERS = [
  { id: 'local',  label: 'local',  model: 'qwen3-30b' },
  { id: 'claude', label: 'claude', model: 'sonnet'    },
  { id: 'codex',  label: 'codex',  model: 'gpt-5.5'  },
  { id: 'gemini', label: 'gemini', model: '2.5-pro'   },
];

/**
 * Behavior contract §2.3 — which providers are in the pool per mode.
 * Local: GPU + CPU (qwen3-30b via llama-server, qwen3:8b via Ollama).
 * Assisted: Local + claude for review only.
 * Hybrid: Local + claude + codex for code execution.
 * Cloud: All providers.
 */
const MODE_PROVIDERS: Record<ModeId, string[]> = {
  local:          ['local'],
  local_enhanced: ['local'],
  assisted:       ['local', 'claude'],
  hybrid:         ['local', 'claude', 'codex'],
  cloud_plus:     ['local', 'claude', 'codex', 'gemini'],
  cloud:          ['local', 'claude', 'codex', 'gemini'],
};

const INFRA = [
  { id: 'git',    label: 'Git'    },
  { id: 'plane',  label: 'Plane'  },
  { id: 'ollama', label: 'Ollama' },
];

// SVG uses CSS custom property values via string injection —
// no raw hex: all colours are CSS variable references.
const TOK = {
  accent:        'var(--accent-primary)',
  accentBg:      'var(--accent-primary-subtle)',
  surface:       'var(--bg-surface)',
  borderSubtle:  'var(--border-subtle)',
  borderEmph:    'var(--border-emphasis)',
  success:       'var(--status-success)',
  warning:       'var(--status-warning)',
  textPrimary:   'var(--text-primary)',
  textTertiary:  'var(--text-tertiary)',
  textAccent:    'var(--text-accent)',
  info:          'var(--status-info)',
};

export function RoutingDiagram({ connections, mode = 'local' }: Props) {
  // Which providers are in the pool for the current mode
  const modeKey = (mode as ModeId) in MODE_PROVIDERS ? (mode as ModeId) : 'local';
  const availableProviders = MODE_PROVIDERS[modeKey];
  const [tooltip, setTooltip] = useState<{ id: string; text: string } | null>(null);

  // Layout constants
  const W = 480, H = 80;
  const conductorW = 72, conductorH = 32;
  const conductorX = 8, conductorY = (H - conductorH) / 2;
  const conductorCX = conductorX + conductorW;
  const conductorCY = conductorY + conductorH / 2;

  // Agent pills — horizontal row
  const pillW = 72, pillH = 24, pillGap = 8;
  const pillsStartX = conductorX + conductorW + 24;
  const pillY = (H - pillH) / 2;
  const totalPillsW = PROVIDERS.length * pillW + (PROVIDERS.length - 1) * pillGap;
  const pillsEndX = pillsStartX + totalPillsW;

  // Infra — right side (hidden on narrow viewports via responsive CSS)
  const infraStartX = pillsEndX + 24;

  return (
    <div className="h-card" style={{ maxHeight: 160 }}>
      <div className="h-card-header" style={{ cursor: 'default', paddingBottom: 'var(--space-2)' }}>
        <span style={{ fontWeight: 600, fontSize: 'var(--text-md)', color: 'var(--text-primary)' }}>
          Agent Routing
        </span>
        {tooltip && (
          <span style={{
            fontSize: 'var(--text-xs)',
            color: 'var(--text-secondary)',
            fontFamily: 'var(--font-mono)',
          }}>
            {tooltip.text}
          </span>
        )}
      </div>

      <div style={{ padding: '0 var(--space-3) var(--space-2)' }}>
        <svg
          viewBox={`0 0 ${W} ${H}`}
          style={{ width: '100%', height: 'auto', maxHeight: 80, display: 'block', overflow: 'visible' }}
          aria-label="Agent routing diagram"
        >
          {/* ── Conductor badge ─────────────────────────────────── */}
          <rect
            x={conductorX} y={conductorY}
            width={conductorW} height={conductorH}
            rx={4}
            fill="rgba(92,224,216,0.08)"
            stroke="rgba(92,224,216,0.5)"
            strokeWidth={1}
          />
          <text x={conductorX + conductorW / 2} y={conductorY + 13}
            textAnchor="middle" fontSize={8} fontWeight="600"
            fill="rgba(92,224,216,1)">
            Harmony
          </text>
          <text x={conductorX + conductorW / 2} y={conductorY + 24}
            textAnchor="middle" fontSize={7}
            fill="rgba(92,224,216,0.6)">
            Conductor
          </text>

          {/* ── Agent pills ─────────────────────────────────────── */}
          {PROVIDERS.map((p, i) => {
            const px = pillsStartX + i * (pillW + pillGap);
            const cx = px + pillW / 2;
            const conn = connections.find(c => c.to === p.id);
            const isActive = !!conn?.active;
            const isWaiting = !!conn?.waiting;
            const isDashed = !!conn?.exhausted;
            // Available = in the provider pool for current mode (even if idle)
            const isAvailable = availableProviders.includes(p.id);
            const isExcluded = !isAvailable;

            const lineColor = isActive
              ? 'rgba(63,185,80,0.7)'
              : isWaiting
                ? 'rgba(210,153,34,0.6)'
                : isAvailable
                  ? 'rgba(92,224,216,0.25)'   // teal dim — in pool, idle
                  : 'rgba(255,255,255,0.05)';  // near-invisible — excluded by mode

            const dotFill = isActive
              ? 'rgba(63,185,80,1)'
              : isWaiting
                ? 'rgba(210,153,34,1)'
                : isAvailable
                  ? 'rgba(92,224,216,0.5)'     // teal dim dot for in-pool idle
                  : 'rgba(80,80,90,0.5)';      // muted dot for excluded

            const pillStroke = isActive
              ? 'rgba(63,185,80,0.5)'
              : isAvailable
                ? 'rgba(92,224,216,0.3)'        // teal dim border — in pool
                : 'rgba(255,255,255,0.06)';     // near-invisible — excluded

            const pillFill = isActive
              ? 'rgba(63,185,80,0.08)'
              : isAvailable
                ? 'rgba(92,224,216,0.06)'       // very subtle teal — in pool
                : 'rgba(255,255,255,0.02)';     // nearly transparent — excluded

            const labelFill = isActive
              ? 'rgba(230,237,243,1)'
              : isAvailable
                ? 'rgba(200,210,220,0.85)'      // slightly dimmer for idle in-pool
                : 'rgba(100,110,120,0.6)';      // dim for excluded

            return (
              <g key={p.id}
                onMouseEnter={() => setTooltip({ id: p.id, text: `${p.label} / ${p.model}${isActive ? ' — active' : isWaiting ? ' — waiting' : isAvailable ? ` — idle (${mode} mode)` : ` — excluded (${mode} mode)`}` })}
                onMouseLeave={() => setTooltip(null)}
                style={{ cursor: 'default' }}
              >
                {/* Connection line from conductor */}
                <line
                  x1={conductorCX} y1={conductorCY}
                  x2={px} y2={pillY + pillH / 2}
                  stroke={lineColor}
                  strokeWidth={isActive ? 1 : 0.7}
                  strokeDasharray={isDashed ? '3,2' : undefined}
                  opacity={isActive || isWaiting ? 1 : 0.5}
                />
                {/* Pill */}
                <rect
                  x={px} y={pillY}
                  width={pillW} height={pillH}
                  rx={4}
                  fill={pillFill}
                  stroke={pillStroke}
                  strokeWidth={1}
                />
                {/* Status dot */}
                <circle cx={px + 10} cy={pillY + pillH / 2} r={3} fill={dotFill} />
                {/* Label + model */}
                <text x={px + 18} y={pillY + 9}
                  fontSize={8} fontWeight={isActive ? '600' : '400'}
                  fill={labelFill}>
                  {p.label}
                </text>
                <text x={px + 18} y={pillY + 19}
                  fontSize={7}
                  fill="rgba(110,118,129,0.8)">
                  {p.model}
                </text>
              </g>
            );
          })}

          {/* ── Infra icons (right side) ─────────────────────────── */}
          {INFRA.map((inf, i) => {
            const iy = 10 + i * 22;
            return (
              <g key={inf.id}>
                <line
                  x1={pillsEndX} y1={pillY + pillH / 2}
                  x2={infraStartX} y2={iy + 7}
                  stroke="rgba(88,166,255,0.2)"
                  strokeWidth={0.7}
                />
                <text x={infraStartX} y={iy + 11}
                  fontSize={8}
                  fill="rgba(88,166,255,0.7)">
                  {inf.label}
                </text>
              </g>
            );
          })}
        </svg>
      </div>
    </div>
  );
}
