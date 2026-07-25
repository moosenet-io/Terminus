// CGUI-04 (TERM #527): the reusable module DETAIL view — "Inside a client" (guide-spec §4).
// The answer to the operator complaint "modules are missing all their tools/settings/depth":
// drilling into any module (via the Overview card's "Configure" / body click, CGUI-03 →
// ModuleCard) opens THIS view — "same shell, deeper zoom". One component serves EVERY module
// so all of them reach the guide's depth checklist:
//   header + Configure/Restart actions · 4 metric tiles incl. TOOLS MOUNTED · position-in-flow
//   node diagram · a real Configuration panel · a live streaming log.
//
// DATA PROVENANCE (real vs placeholder — the item requires calling this out):
//   • SPEND TODAY  — real ($0.00; the whole fleet is local-inference free, guide §0/§3.2).
//   • TOOLS MOUNTED — real for `terminus` (summed from the aggregation client's
//     terminus.configSummary() tool counts); em-dash placeholder for every other module until
//     the CGUI-08 data client exposes a per-module count.
//   • CALLS/HOUR, P50 LATENCY — placeholder em-dash (no per-module metric stream yet, CGUI-08).
//   • POSITION IN FLOW + CONFIGURATION — representative topology/config (moduleMeta.ts), NOT
//     live; every region renders a sensible sample rather than empty, pending CGUI-08.
//   • LIVE LOG — synthetic telemetry (seeded from the module id + live health.detail); a
//     placeholder stream standing in for the real per-module log feed (CGUI-08).
//
// Tokens only (var(--…)); the raw px/hex that survive are DS-parity component/geometry literals
// (9px flow dots, the connector viewBox, brand rgba) matching the posture NodeBadge/StatusPill/
// HarmonyForestPanel already take — adherence-lint runs in warn mode.
import { useEffect, useMemo, useRef, useState } from 'react';
import type { CSSProperties } from 'react';
import { Link } from 'react-router-dom';
import type { ModuleDescriptor } from '../../lib/moduleRegistry';
import type { HealthStatus } from '../../lib/aggregationClient';
import { getAggregationClient } from '../../lib/aggregationClient';
import { PanelRoot } from '../../components/PanelRoot';
import { NodeBadge } from '../../components/NodeBadge';
import { Badge } from '../../components/Badge';
import { Button } from '../../components/Button';
import { Tabs, tabId, tabPanelId } from '../../components/Tabs';
import { StatusPill } from '../../components/StatusPill';
import type { PillState } from '../../components/StatusPill';
import {
  MODULE_META, KIND_COLOR, configForModule, flowForModule,
  type ConfigRow, type BadgeToneName,
} from './moduleMeta';

// ── live prefers-reduced-motion (SMIL + JS-interval streams honour it) ───────────────────────
function useReducedMotion(): boolean {
  const [reduced, setReduced] = useState(() =>
    typeof matchMedia !== 'undefined' && matchMedia('(prefers-reduced-motion: reduce)').matches);
  useEffect(() => {
    if (typeof matchMedia === 'undefined') return;
    const mq = matchMedia('(prefers-reduced-motion: reduce)');
    const on = () => setReduced(mq.matches);
    mq.addEventListener('change', on);
    return () => mq.removeEventListener('change', on);
  }, []);
  return reduced;
}

// ── live-log telemetry model (pure, exported for unit test) ──────────────────────────────────
export interface LogLine {
  /** HH:MM:SS wall-clock stamp. */
  time: string;
  /** `[ok]` (green) for a settled call, `[..]` (blue) for an in-flight one. */
  tag: '[ok]' | '[..]';
  /** The telemetry event, e.g. `invoke gitea.list_repos`. */
  event: string;
  /** Muted trailing cost figure — always 0.00 for the free local fleet. */
  cost: string;
}

/** Representative per-module event verbs the synthetic stream cycles through. Placeholder copy
 *  (no live log feed yet, CGUI-08) — deterministic so a test can assert the shape. */
const EVENT_VERBS = [
  'invoke', 'resolve', 'dispatch', 'compile', 'stream', 'cache', 'route', 'verify',
];
const EVENT_NOUNS = [
  'tool.call', 'health.probe', 'session.open', 'request.route', 'index.scan',
  'briefing.build', 'proxy.forward', 'vault.read',
];

function pad2(n: number): string { return n < 10 ? `0${n}` : `${n}`; }
function stamp(d: Date): string { return `${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`; }

/**
 * Builds one synthetic telemetry line for a module. Pure + deterministic in its `seq` so the
 * stream reads as coherent per-module activity and a unit test can pin its shape. `at` is
 * injectable for testing (defaults to now). NOT live data — see the file header.
 */
export function makeLogLine(moduleId: string, seq: number, at: Date = new Date()): LogLine {
  const verb = EVENT_VERBS[seq % EVENT_VERBS.length];
  const noun = EVENT_NOUNS[(seq * 3 + moduleId.length) % EVENT_NOUNS.length];
  return {
    time: stamp(at),
    tag: seq % 5 === 0 ? '[..]' : '[ok]',
    event: `${verb} ${moduleId}.${noun}`,
    cost: '0.00',
  };
}

/** Seed the log with a short backlog so the panel is never empty on first paint. */
function seedLog(moduleId: string, detail: string | undefined): LogLine[] {
  const now = Date.now();
  const seed: LogLine[] = [];
  for (let i = 6; i >= 1; i--) seed.push(makeLogLine(moduleId, 6 - i, new Date(now - i * 4000)));
  // Fold the one genuinely-live signal we have — the health probe detail — in as the newest line.
  seed.push({ time: stamp(new Date(now)), tag: '[ok]', event: `${moduleId}.health ${detail ?? 'reachable'}`, cost: '0.00' });
  return seed;
}

const LOG_CAP = 40;

// ── small style helpers ──────────────────────────────────────────────────────────────────────
const panel: CSSProperties = {
  borderRadius: 'var(--radius-lg)',
  border: 'var(--border-width) solid var(--line-default)',
  background: 'linear-gradient(180deg,var(--space-700),var(--space-800))',
  boxShadow: 'var(--shadow-md), var(--inset-hi)',
};
const panelHeader: CSSProperties = {
  fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', letterSpacing: '.16em',
  textTransform: 'uppercase', color: 'var(--text-400)',
};
const monoFigure: CSSProperties = { fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-h4)', color: 'var(--text-100)' };
const tileLabel: CSSProperties = {
  fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', letterSpacing: 'var(--ls-mono)',
  textTransform: 'uppercase', color: 'var(--text-500)', marginTop: 'var(--space-2)',
};

interface MetricTile { label: string; figure: string; real: boolean; }

/** A single 100×40 connector: an accent-tinted line (0.3–0.5 opacity, guide §4) with a
 *  traveling pulse dot (SMIL animateMotion, withheld under reduced motion). `gradient` renders
 *  the violet→green core→endpoint feed. */
function Connector({ gradient, reduced }: { gradient?: boolean; reduced: boolean }) {
  const gid = gradient ? 'flowgrad' : undefined;
  return (
    <svg viewBox="0 0 100 40" preserveAspectRatio="none" width="100%" height="40" style={{ flex: 1, minWidth: 28, overflow: 'visible' }} aria-hidden>
      {gradient && (
        <defs>
          <linearGradient id={gid} x1="0" y1="0" x2="1" y2="0">
            {/* brand violet → green data-flow gradient (DS §4 literal paints) */}
            <stop offset="0%" stopColor="#7C3AED" stopOpacity="0.45" />
            <stop offset="100%" stopColor="#10B981" stopOpacity="0.45" />
          </linearGradient>
        </defs>
      )}
      <path id="connpath" d="M0 20 L100 20" fill="none"
        stroke={gradient ? `url(#${gid})` : 'var(--violet-500)'}
        strokeOpacity={gradient ? 1 : 0.4} strokeWidth={1.5} strokeLinecap="round" />
      {!reduced && (
        <circle r={2.4} fill={gradient ? '#34D399' : '#7C3AED'}>
          <animateMotion dur="3.2s" repeatCount="indefinite" path="M0 20 L100 20" />
        </circle>
      )}
    </svg>
  );
}

const PILL_FOR: Record<'online' | 'idle' | 'error', { state: PillState; label: string }> = {
  online: { state: 'online', label: 'online' },
  idle: { state: 'idle', label: 'idle' },
  error: { state: 'error', label: 'error' },
};

interface ModuleDetailProps {
  module: ModuleDescriptor;
  health?: HealthStatus;
}

/**
 * The reusable "Inside a client" detail view. Rendered for every module by App.tsx's
 * `/:moduleId/detail` route; the module rail stays mounted (the first path segment is the
 * module id) so the shell frame never moves — only this canvas does.
 */
export function ModuleDetail({ module, health }: ModuleDetailProps) {
  const reduced = useReducedMotion();
  const meta = MODULE_META[module.id];
  const kindColor = KIND_COLOR[meta.kind];
  const flow = useMemo(() => flowForModule(module.id, module.title), [module.id, module.title]);
  const config = useMemo(() => configForModule(module.id), [module.id]);

  const healthState: 'online' | 'idle' | 'error' = health?.available === false ? 'error' : 'online';
  const pill = PILL_FOR[healthState];

  // POL-09 (§3.4): the detail view is tabbed (Overview / Config / Flow / Logs) rather than one
  // long scroll, matching professional resource-detail panes. The metric strip stays pinned
  // above the tabs (key vitals are always visible); each tab zooms one region.
  const [activeTab, setActiveTab] = useState<'overview' | 'config' | 'flow' | 'logs'>('overview');

  // POL-09 FIX (review): full tablist/tab/tabpanel ARIA wiring. Each rendered panel gets
  // role="tabpanel" + a matching id + aria-labelledby back to its tab; the tab carries
  // aria-controls to this id (in Tabs). idBase namespaces the ids to this view.
  const TAB_ID_BASE = 'module-detail';
  const tabPanelAttrs = (id: string) => ({
    id: tabPanelId(TAB_ID_BASE, id),
    role: 'tabpanel' as const,
    'aria-labelledby': tabId(TAB_ID_BASE, id),
    tabIndex: 0,
  });

  // TOOLS MOUNTED — the one metric we can source live today, and only for the terminus tool
  // hub: sum the aggregation client's per-terminus-module tool counts. Every other module has
  // no per-module tool count exposed yet (CGUI-08) → em-dash placeholder.
  const [toolCount, setToolCount] = useState<number | null>(null);
  useEffect(() => {
    if (module.id !== 'terminus') { setToolCount(null); return; }
    let live = true;
    getAggregationClient().terminus.configSummary()
      .then(s => { if (live) setToolCount(s.modules.reduce((n, m) => n + (m.toolCount ?? 0), 0)); })
      .catch(() => { if (live) setToolCount(null); });
    return () => { live = false; };
  }, [module.id]);

  const tools = module.id === 'terminus' ? toolCount : null;
  const metrics: MetricTile[] = [
    { label: 'calls/hour', figure: '—', real: false },
    { label: 'p50 latency', figure: '—', real: false },
    { label: 'tools mounted', figure: tools != null ? String(tools) : '—', real: tools != null },
    { label: 'spend today', figure: '$0.00', real: true },
  ];

  // ── live streaming log ─────────────────────────────────────────────────────────────────────
  const [log, setLog] = useState<LogLine[]>(() => seedLog(module.id, health?.detail));
  const seqRef = useRef(7); // seedLog consumed 0..6
  const logScrollRef = useRef<HTMLDivElement | null>(null);

  // Reset the stream when the operator drills into a different module.
  useEffect(() => {
    setLog(seedLog(module.id, health?.detail));
    seqRef.current = 7;
    // health.detail intentionally NOT a dep — a health refresh shouldn't wipe the running log.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [module.id]);

  useEffect(() => {
    if (reduced) return; // reduced motion → static seed, no streaming (guide §0/§6)
    const iv = setInterval(() => {
      setLog(prev => {
        const next = [...prev, makeLogLine(module.id, seqRef.current++)];
        return next.length > LOG_CAP ? next.slice(next.length - LOG_CAP) : next;
      });
    }, 2600);
    return () => clearInterval(iv);
  }, [module.id, reduced]);

  // Keep the newest line in view (append-at-bottom, guide §4 "streaming/scrolling").
  useEffect(() => {
    const el = logScrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [log]);

  // Extracted so the Overview tab (compact, 2-col) and the dedicated Flow/Config tabs
  // (full-width) render the SAME section without duplicating markup.
  const flowSection = (
    <section style={{ ...panel, padding: 'var(--space-4)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <div style={panelHeader}>Position in flow</div>
      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)' }}>
        <NodeBadge name={flow.source.name} role={flow.source.role} kind={flow.source.kind} />
        <Connector reduced={reduced} />
        <NodeBadge name={flow.core.name} role={flow.core.role} kind={flow.core.kind} pulse={!reduced} />
        <Connector gradient reduced={reduced} />
        <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)' }}>
          {flow.endpoints.map(ep => (
            <NodeBadge key={ep.name} name={ep.name} role={ep.role} kind={ep.kind} />
          ))}
        </div>
      </div>
    </section>
  );

  const configSection = (
    <section style={{ ...panel, padding: 'var(--space-4)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <div style={panelHeader}>Configuration</div>
      <div style={{ display: 'flex', flexDirection: 'column' }}>
        {config.map((row: ConfigRow, i) => (
          <div key={row.key} style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 'var(--space-3)', padding: 'var(--space-3) 0', borderTop: i === 0 ? undefined : 'var(--border-width) solid var(--line-soft)' }}>
            <span style={{ fontFamily: 'var(--font-sans)', fontSize: 'var(--fs-sm)', color: 'var(--text-300)' }}>{row.key}</span>
            {row.badge
              ? <Badge tone={row.badge.tone as BadgeToneName} mono>{row.badge.label}</Badge>
              : <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-100)' }}>{row.value}</span>}
          </div>
        ))}
      </div>
    </section>
  );

  return (
    <PanelRoot style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-5)', padding: 'var(--space-5)' }}>
      {/* ===== HEADER ===== */}
      <header style={{ display: 'flex', alignItems: 'flex-start', justifyContent: 'space-between', gap: 'var(--space-4)', flexWrap: 'wrap' }}>
        <div style={{ minWidth: 0 }}>
          {/* Way back to the overview (the shell rail also stays mounted — this is the explicit
              breadcrumb the item asks for). */}
          <Link to="/overview" style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', letterSpacing: 'var(--ls-mono)', color: 'var(--text-400)', textDecoration: 'none' }}>
            ‹ overview
          </Link>
          <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-3)', marginTop: 'var(--space-2)' }}>
            {/* kind node-dot — 9px + 8px glow is DS-parity dot geometry (matches NodeBadge). */}
            <span aria-hidden style={{ width: 9, height: 9, borderRadius: '50%', background: kindColor, boxShadow: `0 0 8px ${kindColor}`, flexShrink: 0 }} />
            <h2 style={{ margin: 0, fontFamily: 'var(--font-sans)', fontWeight: 'var(--fw-semibold)', fontSize: 'var(--fs-h3)', color: 'var(--text-100)' }}>
              {module.title}
            </h2>
            <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', letterSpacing: 'var(--ls-mono)', color: kindColor }}>{meta.role}</span>
            <StatusPill state={pill.state} label={pill.label} />
          </div>
          <div style={{ marginTop: 'var(--space-2)', fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-300)' }}>
            {meta.desc}
          </div>
        </div>
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)' }}>
          {/* Configure/Restart are the guide §4 header actions. No live control surface is wired
              yet (CGUI-08) — Configure deep-links to the module's config panel when one exists;
              Restart is a placeholder affordance (disabled) pending the ops endpoint. */}
          <Button variant="secondary" size="sm" disabled title="Module configuration lands with the CGUI-08 data client">
            Configure
          </Button>
          <Button variant="secondary" size="sm" disabled title="Restart wiring lands with the CGUI-08 ops endpoint">
            Restart
          </Button>
        </div>
      </header>

      {/* ===== 4 METRIC TILES ===== */}
      <div style={{ ...panel, display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(140px, 1fr))', overflow: 'hidden' }}>
        {metrics.map((m, i) => (
          <div key={m.label} style={{ padding: 'var(--space-4)', borderLeft: i === 0 ? undefined : 'var(--border-width) solid var(--line-soft)' }}>
            <div style={{ ...monoFigure, color: m.figure === '—' ? 'var(--text-500)' : (m.label === 'spend today' ? 'var(--flux-green)' : 'var(--text-100)') }}>
              {m.figure}
            </div>
            <div style={tileLabel}>{m.label}</div>
          </div>
        ))}
      </div>

      {/* ===== TABS (§3.4) ===== */}
      <Tabs
        idBase={TAB_ID_BASE}
        aria-label={`${module.title} detail sections`}
        activeId={activeTab}
        onSelect={id => setActiveTab(id as typeof activeTab)}
        tabs={[
          { id: 'overview', label: 'Overview' },
          { id: 'config', label: 'Config' },
          { id: 'flow', label: 'Flow' },
          { id: 'logs', label: 'Logs', badge: String(log.length) },
        ]}
      />

      {/* ===== OVERVIEW TAB — the at-a-glance: flow diagram + configuration side by side ===== */}
      {activeTab === 'overview' && (
        <div {...tabPanelAttrs('overview')} style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(300px, 1fr))', gap: 'var(--space-5)' }}>
          {flowSection}
          {configSection}
        </div>
      )}

      {/* ===== FLOW TAB — the position-in-flow diagram, full width ===== */}
      {activeTab === 'flow' && <div {...tabPanelAttrs('flow')}>{flowSection}</div>}

      {/* ===== CONFIG TAB — the configuration panel, full width ===== */}
      {activeTab === 'config' && <div {...tabPanelAttrs('config')}>{configSection}</div>}

      {/* ===== LOGS TAB — the live streaming log ===== */}
      {activeTab === 'logs' && (
        <section {...tabPanelAttrs('logs')} style={{ ...panel, display: 'flex', flexDirection: 'column', minHeight: 220 }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', padding: 'var(--space-3) var(--space-4) var(--space-2)' }}>
            {/* green header dot (guide §4) — 7px + glow, DS-parity geometry. */}
            <span aria-hidden style={{ width: 7, height: 7, borderRadius: '50%', background: 'var(--flux-green)', boxShadow: 'var(--glow-green)', flexShrink: 0 }} />
            <span style={panelHeader}>Live log — {module.title}</span>
          </div>
          <div
            ref={logScrollRef}
            className="hf-scroll"
            role="log"
            aria-live="polite"
            aria-relevant="additions"
            aria-label={`Live log — ${module.title}`}
            style={{ flex: 1, minHeight: 0, maxHeight: 360, overflowY: 'auto', padding: '0 var(--space-4) var(--space-4)', display: 'flex', flexDirection: 'column', gap: 'var(--space-1)', fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', lineHeight: 1.6 }}
          >
            {log.map((ln, i) => (
              <div key={i}>
                <span style={{ color: 'var(--text-500)' }}>{ln.time}</span>{' '}
                <span style={{ color: ln.tag === '[ok]' ? 'var(--flux-green)' : 'var(--flux-blue)' }}>{ln.tag}</span>{' '}
                <span style={{ color: 'var(--text-200)' }}>{ln.event}</span>{' '}
                <span style={{ color: 'var(--text-500)' }}>cost={ln.cost}</span>
              </div>
            ))}
          </div>
        </section>
      )}
    </PanelRoot>
  );
}
