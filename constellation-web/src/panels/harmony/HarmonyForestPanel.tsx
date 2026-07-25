// CGUI-11 (TERM #534): Harmony Forest Build — the autonomous-build-orchestrator screen (design
// handoff §2, the PRIMARY design). A spec "grows" as an SVG tree: each leaf is an issue, each
// bough a work cluster, the whole tree one git branch, and finished trees drift to a persisted
// horizon "forest" (localStorage, last 80). Driven entirely by the self-contained simulation in
// forestEngine.ts (not live data in this item). Recreates the prototype's visual output in
// idiomatic React — the proprietary DCLogic/sc-for/x-import runtime is replaced with a plain
// class engine + a tick interval + React.createElement SVG builders.
//
// Tokens are used for all UI chrome; literal paints survive ONLY inside the <svg> forest/tree
// (the DS-canonical viz greens #1c9160/#178050, violet sap #7C3AED, and the amber/rose/green
// leaf-state colors) and a few brand-glow rgba values, exactly as the design specifies — these
// are SVG fills, not tokenized UI chrome. Raw px are the design's exact geometry (600px body,
// 400px panel, 190px log, the 295 235 410 490 viewBox) — same pixel-parity posture the DS
// primitives (Card/NodeBadge/StatusPill) already take. adherence-lint runs in warn mode.
import { createElement, useEffect, useReducer, useRef, useState, type CSSProperties, type ReactElement } from 'react';
import { StatusPill, type PillState } from '../../components/StatusPill';
import {
  ForestEngine, SPECS, STAGES, TYPE_DOT, TICK_MS, hashStr, mulberry32, leafPath,
  type Issue, type Phase,
} from './forestEngine';
import './forest.css';

const h = createElement;

/** Live `prefers-reduced-motion` flag — used to withhold the SVG SMIL sap/halo pulses, which the
 *  app's CSS reduced-motion rule can't reach (SMIL animation isn't a CSS animation). */
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

// ── phase copy (§2.3 phase banner) ──────────────────────────────────────────────────────────
const PHASE: Record<Phase, { label: string; sub: (e: ForestEngine) => string; color: string; glow: string }> = {
  idle:     { label: 'Ready to grow',            sub: () => 'Load a spec and press Grow spec',                     color: 'var(--text-200)',        glow: 'rgba(124,58,237,0.3)' },
  building: { label: 'Growing',                  sub: e => `${e.done} of ${e.issues.length} issues merged`,       color: 'var(--flux-amber)',      glow: 'rgba(245,158,11,0.4)' },
  commit:   { label: 'Branch complete — merging', sub: () => 'all issues committed',                              color: 'var(--flux-green-soft)', glow: 'rgba(16,185,129,0.55)' },
  joining:  { label: 'Joining the forest',       sub: () => 'the project grows',                                  color: 'var(--flux-green-soft)', glow: 'rgba(16,185,129,0.5)' },
  shipped:  { label: 'Shipped',                  sub: e => `${e.shipped} specs in the forest`,                    color: 'var(--flux-green-soft)', glow: 'rgba(16,185,129,0.5)' },
};

function leafColor(st: Issue['status']): string {
  return st === 'done' ? '#10B981' : st === 'failed' ? '#F43F5E' : st === 'building' ? '#F59E0B' : '#3a3550';
}

// ── SVG builders (ports of the prototype's buildFocusSvg / buildForestSvg) ───────────────────

function buildFocusSvg(e: ForestEngine, reduced: boolean): ReactElement | null {
  const t = e.tree;
  if (!t) return null;
  const joinT = e.phase === 'joining';
  const kids: ReactElement[] = [];

  kids.push(h('ellipse', { key: 'seat', cx: 500, cy: 668, rx: 120, ry: 16, fill: 'rgba(16,185,129,0.12)' }));

  kids.push(h('path', {
    key: 'trunk', d: t.trunk.d, fill: 'none', stroke: '#6a577f', strokeWidth: 15, strokeLinecap: 'round',
    pathLength: 1, strokeDasharray: 1,
    style: { strokeDashoffset: e.trunkGrown ? 0 : 1, transition: 'stroke-dashoffset .9s var(--ease-out)', filter: 'drop-shadow(0 0 6px rgba(124,58,237,0.35))' },
  }));
  if (e.trunkGrown && !reduced) {
    kids.push(h('circle', { key: 'trunksap', r: 3, fill: '#7C3AED', opacity: 0.85 },
      h('animateMotion', { dur: '3s', repeatCount: 'indefinite', path: t.trunk.d, rotate: 'auto' })));
  }

  t.boughs.forEach(b => {
    const rev = !!e.revealed[b.id];
    kids.push(h('path', {
      key: b.id, d: b.d, fill: 'none', stroke: '#5c4c7d', strokeWidth: 5.5, strokeLinecap: 'round',
      pathLength: 1, strokeDasharray: 1,
      style: { strokeDashoffset: rev ? 0 : 1, transition: 'stroke-dashoffset .7s var(--ease-out)', filter: 'drop-shadow(0 0 4px rgba(124,58,237,0.3))' },
    }));
    if (rev && !reduced) {
      kids.push(h('circle', { key: b.id + 's', r: 2.6, fill: '#7C3AED', opacity: 0.9 },
        h('animateMotion', { dur: b.dur + 's', repeatCount: 'indefinite', path: b.d })));
    }
    if (rev) {
      b.fill.forEach((f, i) => {
        kids.push(h('g', { key: b.id + 'f' + i, transform: `translate(${f.x} ${f.y}) rotate(${f.ang})`, style: { animation: 'tg-leafpop .5s var(--ease-out) both' } },
          h('path', { d: leafPath(f.sz), fill: f.g ? '#1c9160' : '#178050' }),
          h('path', { d: `M0 ${(0.5 * f.sz).toFixed(1)} L0 ${(-0.82 * f.sz).toFixed(1)}`, stroke: 'rgba(210,255,230,0.22)', strokeWidth: 0.6 })));
      });
    }
  });

  // canopy glow — grows with the fraction of non-queued issues
  const lit = e.issues.filter(i => i.status !== 'queued').length;
  const frac = e.issues.length ? lit / e.issues.length : 0;
  if (frac > 0) {
    kids.push(h('ellipse', { key: 'crown', cx: 500, cy: 350, rx: 265, ry: 195, fill: 'rgba(16,185,129,0.13)', opacity: frac, style: { transition: 'opacity .8s var(--ease-out)' } }));
    kids.push(h('ellipse', { key: 'crown2', cx: 500, cy: 360, rx: 170, ry: 135, fill: 'rgba(52,211,153,0.10)', opacity: frac, style: { transition: 'opacity .8s var(--ease-out)' } }));
  }

  // leaves — lush multi-leaflet clusters
  e.issues.forEach(it => {
    if (it.status === 'queued') return;
    const done = it.status === 'done';
    const c = leafColor(it.status);
    const active = it.status === 'building' || it.status === 'failed';
    const lr = mulberry32(hashStr(it.leaf.id) * 7 + 13);
    const cluster: [number, number, number][] = [[0, 0, done ? 9 : 7]];
    if (done) {
      cluster.push([-11 + lr() * 4, -8 - lr() * 4, 6.6]); cluster.push([10 + lr() * 4, -7 - lr() * 4, 6.2]);
      cluster.push([-3 + lr() * 4, 10 + lr() * 4, 5.8]); cluster.push([12 + lr() * 4, 4 + lr() * 4, 5.2]);
      cluster.push([-12 + lr() * 4, 3 + lr() * 3, 4.8]); cluster.push([2 + lr() * 5, -13 - lr() * 3, 4.8]);
      cluster.push([16 + lr() * 4, -2 + lr() * 4, 4]);
    } else if (active) {
      cluster.push([-8 + lr() * 3, -5, 5.2]); cluster.push([8, -4 + lr() * 3, 4.6]); cluster.push([0, 8, 4.2]);
    }
    const parts: ReactElement[] = [
      h('circle', { key: 'halo', r: done ? 21 : 16, fill: done ? '#10B981' : c, opacity: done ? 0.2 : 0.16 },
        active && !reduced ? h('animate', { attributeName: 'opacity', values: '0.1;0.32;0.1', dur: '1.5s', repeatCount: 'indefinite' }) : null),
    ];
    cluster.forEach((L, i) => {
      const col = done ? (i % 2 ? '#34D399' : '#10B981') : c;
      const sz = L[2] * 1.7;
      const ang = (Math.atan2(L[1], L[0]) * 180 / Math.PI + 90 + (lr() - 0.5) * 46).toFixed(1);
      parts.push(h('g', { key: 'l' + i, transform: `translate(${L[0]} ${L[1]}) rotate(${ang})` },
        h('path', { d: leafPath(sz), fill: col, style: { transition: 'fill .5s var(--ease-out)' } }),
        h('path', { d: `M0 ${(0.5 * sz).toFixed(1)} L0 ${(-0.85 * sz).toFixed(1)}`, stroke: done ? 'rgba(6,50,34,0.55)' : 'rgba(255,255,255,0.3)', strokeWidth: 0.8 }),
        h('path', { d: leafPath(sz), fill: '#f0fff8', opacity: 0.12 })));
    });
    kids.push(h('g', { key: it.leaf.id, transform: `translate(${it.leaf.x} ${it.leaf.y})` },
      h('g', { style: { animation: 'tg-leafpop .6s var(--ease-out) both' } }, parts)));
  });

  const g = h('g', {
    style: {
      transform: joinT ? 'translate(220px,-190px) scale(0.12)' : 'none',
      opacity: joinT ? 0 : 1,
      transformOrigin: '500px 660px',
      transition: 'transform 1.8s var(--ease-out), opacity 1.8s var(--ease-out)',
    },
  }, kids);
  return h('svg', { viewBox: '295 235 410 490', preserveAspectRatio: 'xMidYMax meet', width: '100%', height: '100%', style: { overflow: 'visible' } }, g);
}

function buildForestSvg(e: ForestEngine): ReactElement {
  const trees = [...e.forest].sort((a, b) => a.sc - b.sc);
  // ambient far treeline so the horizon is never empty (26 trees, deterministic seed)
  const arng = mulberry32(20260718);
  const ambient: ReactElement[] = [];
  for (let i = 0; i < 26; i++) {
    const x = 18 + (i / 25) * 964 + (arng() - 0.5) * 22;
    const sc = 0.28 + arng() * 0.16;
    const base = 232 - arng() * 8;
    const cvs: ReactElement[] = [];
    for (let k = 0; k < 3; k++) cvs.push(h('circle', { key: k, cx: x + (arng() - 0.5) * 20 * sc, cy: base - 22 * sc - arng() * 14 * sc, r: (13 + arng() * 8) * sc, fill: '#0c211a', opacity: 0.7 }));
    ambient.push(h('g', { key: 'amb' + i, opacity: 0.6 }, h('rect', { x: x - 1.4 * sc, y: base - 20 * sc, width: 2.8 * sc, height: 20 * sc, fill: '#1c2436' }), ...cvs));
  }
  const shipTrees = trees.map((f, i) => {
    const rng = mulberry32(f.s);
    const base = 200 - f.sc * 30 + f.y * 24;
    const x = 1000 * (f.x / 100);
    const canopy: ReactElement[] = [];
    const cc = f.sc > 0.85 ? '#123326' : '#0f2a20';
    for (let k = 0; k < 5; k++) canopy.push(h('circle', { key: k, cx: x + (rng() - 0.5) * 34 * f.sc, cy: base - 30 * f.sc - rng() * 24 * f.sc, r: (16 + rng() * 12) * f.sc, fill: cc, opacity: 0.92 }));
    return h('g', { key: 'f' + i + '_' + f.s, style: f.isNew ? { animation: 'tg-grow .9s var(--ease-out) both', transformBox: 'fill-box', transformOrigin: 'center bottom' } : undefined },
      h('ellipse', { cx: x, cy: base - 34 * f.sc, rx: 30 * f.sc, ry: 26 * f.sc, fill: 'rgba(16,185,129,0.13)' }),
      h('rect', { x: x - 2 * f.sc, y: base - 30 * f.sc, width: 4 * f.sc, height: 30 * f.sc, fill: '#2b2440' }),
      ...canopy,
      h('circle', { cx: x - 4 * f.sc, cy: base - 46 * f.sc, r: 2.4 * f.sc, fill: '#34D399', opacity: 0.7 }));
  });
  return h('svg', { viewBox: '0 0 1000 260', preserveAspectRatio: 'xMidYMax meet', width: '100%', height: '100%', style: { overflow: 'visible' } }, [...ambient, ...shipTrees]);
}

// ── small style helpers ─────────────────────────────────────────────────────────────────────
const monoLabel: CSSProperties = { fontFamily: 'var(--font-mono)', fontSize: '10px', letterSpacing: '.14em', color: 'var(--text-500)' };
const secondaryBtn: CSSProperties = {
  fontFamily: 'var(--font-sans)', fontWeight: 600, fontSize: '13px', padding: '8px 15px',
  borderRadius: 'var(--radius-md)', cursor: 'pointer', color: 'var(--text-200)',
  border: '1px solid var(--border-strong)', background: 'linear-gradient(180deg,var(--space-600),var(--space-700))',
};
const objectCard: CSSProperties = {
  padding: '18px', borderRadius: 'var(--radius-lg)', border: '1px solid var(--line-default)',
  background: 'linear-gradient(180deg,var(--space-700),var(--space-800))', boxShadow: 'var(--shadow-md), var(--inset-hi)',
};

function pillFor(it: Issue): { state: PillState; label: string } {
  if (it.status === 'done') return { state: 'online', label: 'done' };
  if (it.status === 'failed') return { state: 'error', label: 'failed' };
  if (it.status === 'building') return { state: 'warm', label: STAGES[it.stage] };
  return { state: 'idle', label: 'queued' };
}

// ── the panel ────────────────────────────────────────────────────────────────────────────────
export function HarmonyForestPanel() {
  const reduced = useReducedMotion();
  const [, forceRender] = useReducer((n: number) => n + 1, 0);
  const engineRef = useRef<ForestEngine | null>(null);
  if (engineRef.current === null) engineRef.current = new ForestEngine();
  const engine = engineRef.current;

  useEffect(() => {
    engine.setOnChange(forceRender);
    engine.loadForest();
    engine.loadSpec('auth-service', 12);
    const iv = setInterval(() => engine.tickIfRunning(), TICK_MS);
    return () => { clearInterval(iv); engine.destroy(); };
    // engineRef instance is stable for the component's lifetime.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const ph = PHASE[engine.phase] ?? PHASE.idle;
  const phaseLabel = engine.phase === 'building' ? `Growing ${engine.specName}` : ph.label;
  const logs = [...engine.logsArr].slice(-40).reverse();
  const simLabel = engine.phase === 'building' ? 'Growing…' : (engine.phase === 'shipped' || engine.done > 0 ? 'Grow again' : 'Grow spec');

  return (
    <div style={{ position: 'relative', flex: 1, minHeight: 0, display: 'flex', flexDirection: 'column', overflowY: 'auto' }} className="hf-scroll">
      {/* deep-space + starfield are provided by the shell's page background; the panel adds the
          forest ground-tint via the grove canvas gradients below. */}

      {/* ===== HEADER / CONTROLS ===== */}
      <header style={{ display: 'flex', alignItems: 'center', gap: '22px', padding: '16px 26px', borderBottom: '1px solid var(--line-default)', background: 'linear-gradient(180deg,rgba(22,17,44,0.72),rgba(13,11,26,0.4))', backdropFilter: 'blur(8px)', flexWrap: 'wrap' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: '12px' }}>
          <span style={{ width: '26px', height: '26px', borderRadius: '50%', background: 'radial-gradient(circle,#34D399,#10B981 60%,#7C3AED 130%)', boxShadow: '0 0 16px rgba(16,185,129,0.5)', flex: 'none' }} />
          <div style={{ lineHeight: 1.1 }}>
            <div style={{ fontWeight: 600, fontSize: '16px', letterSpacing: '.04em', color: 'var(--text-100)' }}>Harmony</div>
            <div style={{ fontFamily: 'var(--font-mono)', fontSize: '10px', letterSpacing: '.14em', textTransform: 'uppercase', color: 'var(--text-400)' }}>Autonomous build orchestrator</div>
          </div>
        </div>

        <div style={{ display: 'flex', alignItems: 'center', gap: '8px', paddingLeft: '16px', borderLeft: '1px solid var(--line-soft)' }}>
          <span style={monoLabel}>SPEC</span>
          <div style={{ display: 'flex', gap: '3px' }}>
            {SPECS.map(sp => {
              const activeSpec = engine.specName === sp.name;
              return (
                <button key={sp.name} onClick={() => engine.selectSpec(sp.name, sp.issues)}
                  style={{ fontFamily: 'var(--font-mono)', fontSize: '11px', padding: '6px 11px', borderRadius: 'var(--radius-sm)', cursor: 'pointer', border: `1px solid ${activeSpec ? 'var(--line-strong)' : 'var(--line-soft)'}`, background: activeSpec ? 'rgba(124,58,237,0.2)' : 'transparent', color: activeSpec ? 'var(--violet-200)' : 'var(--text-300)', transition: 'all var(--dur-fast) var(--ease-out)' }}>
                  {sp.name}<span style={{ opacity: 0.6 }}> · {sp.issues}</span>
                </button>
              );
            })}
          </div>
        </div>

        <div style={{ display: 'flex', alignItems: 'center', gap: '9px' }}>
          <span style={monoLabel}>SIZE</span>
          <input type="range" min={6} max={48} step={2} value={engine.size} onChange={e => engine.setSize(+e.target.value)} style={{ width: '96px' }} />
          <span style={{ fontFamily: 'var(--font-mono)', fontSize: '12px', color: 'var(--violet-200)', width: '56px' }}>{engine.size} issues</span>
        </div>

        <div style={{ display: 'flex', alignItems: 'center', gap: '9px' }}>
          <span style={monoLabel}>RATE</span>
          <input type="range" min={0.5} max={2.5} step={0.1} defaultValue={engine.speed} onChange={e => engine.setSpeed(+e.target.value)} style={{ width: '84px' }} />
        </div>

        <div style={{ marginLeft: 'auto', display: 'flex', alignItems: 'center', gap: '10px' }}>
          <span onClick={() => engine.toggleAnnotate()} role="button" tabIndex={0}
            style={{ fontFamily: 'var(--font-mono)', fontSize: '11px', padding: '7px 12px', borderRadius: 'var(--radius-sm)', cursor: 'pointer', border: `1px solid ${engine.annotate ? 'var(--line-strong)' : 'var(--line-soft)'}`, background: engine.annotate ? 'rgba(124,58,237,0.2)' : 'transparent', color: engine.annotate ? 'var(--violet-200)' : 'var(--text-300)' }}>Spec layer</span>
          <span onClick={() => engine.reset()} role="button" tabIndex={0} style={secondaryBtn}>Reset</span>
          {engine.phase === 'building' && (
            <span onClick={() => engine.togglePause()} role="button" tabIndex={0} style={{ ...secondaryBtn, color: 'var(--text-100)' }}>{engine.running ? 'Pause' : 'Resume'}</span>
          )}
          <span onClick={() => engine.grow()} role="button" tabIndex={0}
            style={{ fontFamily: 'var(--font-sans)', fontWeight: 600, fontSize: '13px', padding: '8px 18px', borderRadius: 'var(--radius-md)', cursor: 'pointer', color: 'var(--accent-on)', background: 'var(--grad-accent)', boxShadow: 'var(--glow-violet-soft)' }}>{simLabel}</span>
        </div>
      </header>

      {/* ===== BODY (fixed 600px, grid 1fr / 400px) ===== */}
      <div style={{ height: '600px', display: 'grid', gridTemplateColumns: '1fr 400px', gridTemplateRows: '600px', minHeight: 0, overflow: 'hidden' }}>

        {/* GROVE CANVAS */}
        <div style={{ position: 'relative', overflow: 'hidden', minHeight: 0 }}>
          {/* distant forest band (top 46%) */}
          <div style={{ position: 'absolute', top: 0, left: 0, right: 0, height: '46%', pointerEvents: 'none' }}>
            <div style={{ position: 'absolute', inset: 0 }}>{buildForestSvg(engine)}</div>
            <div style={{ position: 'absolute', left: 0, right: 0, bottom: 0, height: '64%', background: 'linear-gradient(180deg,transparent,rgba(15,26,24,0.55) 60%,rgba(13,11,26,0.85))' }} />
            <div style={{ position: 'absolute', left: 0, right: 0, bottom: 0, height: '40%', background: 'radial-gradient(120% 90% at 50% 100%, rgba(16,185,129,0.10), transparent 70%)', animation: 'tg-fog 14s var(--ease-in-out) infinite' }} />
          </div>

          <div style={{ position: 'absolute', top: '16px', left: '24px', fontFamily: 'var(--font-mono)', fontSize: '10px', letterSpacing: '.16em', color: 'var(--text-500)' }}>
            DISTANT FOREST — {engine.shipped} SPECS SHIPPED
          </div>

          {/* focus tree */}
          <div style={{ position: 'absolute', inset: 0 }}>{buildFocusSvg(engine, reduced)}</div>

          {/* ground glow */}
          <div style={{ position: 'absolute', left: 0, right: 0, bottom: 0, height: '120px', pointerEvents: 'none', background: 'radial-gradient(60% 120% at 50% 100%, rgba(16,185,129,0.14), transparent 70%)' }} />

          {/* phase banner */}
          <div style={{ position: 'absolute', left: '50%', bottom: '26px', transform: 'translateX(-50%)', display: 'flex', flexDirection: 'column', alignItems: 'center', gap: '6px', textAlign: 'center', pointerEvents: 'none' }}>
            <div style={{ fontFamily: 'var(--font-sans)', fontWeight: 600, fontSize: '18px', color: ph.color, textShadow: `0 0 18px ${ph.glow}` }}>{phaseLabel}</div>
            <div style={{ fontFamily: 'var(--font-mono)', fontSize: '11px', color: 'var(--text-300)' }}>{ph.sub(engine)}</div>
          </div>

          {/* legend */}
          <div style={{ position: 'absolute', top: '14px', right: '20px', display: 'flex', flexDirection: 'column', gap: '7px', padding: '12px 14px', borderRadius: 'var(--radius-md)', border: '1px solid var(--line-default)', background: 'rgba(19,15,38,0.66)', backdropFilter: 'blur(6px)' }}>
            <div style={{ fontFamily: 'var(--font-mono)', fontSize: '9px', letterSpacing: '.16em', color: 'var(--text-500)' }}>LEAF = ISSUE</div>
            {([['var(--flux-amber)', 'building'], ['var(--flux-green)', 'done'], ['var(--flux-rose)', 'failed'], ['var(--violet-500)', 'sap · flow']] as const).map(([col, txt]) => (
              <div key={txt} style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
                <span style={{ width: '9px', height: '9px', borderRadius: '50%', background: col, boxShadow: `0 0 8px ${col}` }} />
                <span style={{ fontSize: '11px', color: 'var(--text-200)' }}>{txt}</span>
              </div>
            ))}
          </div>

          {/* annotation layer (Spec layer toggle) */}
          {engine.annotate && (
            <div style={{ position: 'absolute', inset: 0, pointerEvents: 'none' }}>
              {([
                { top: '32%', left: '8%', right: undefined, mw: '170px', eb: 'var(--flux-green-soft)', k: 'LEAF · Issue', d: 'One completed work item. Pops green when its 5-step pipeline commits.', delay: '.4s' },
                { top: '56%', left: '6%', right: undefined, mw: '170px', eb: 'var(--violet-300)', k: 'BOUGH + SAP', d: 'A bough grows in as its first issue starts; violet sap pulses feed active work.', delay: '.5s' },
                { top: '70%', left: '47%', right: undefined, mw: '150px', eb: 'var(--text-300)', k: 'TREE · Branch', d: 'The whole tree is one git branch. Complete = merged.', delay: '.6s' },
                { top: '8%', left: undefined, right: '26%', mw: '180px', eb: 'var(--flux-green-soft)', k: 'FOREST · Spec / epic', d: 'Each finished tree drifts to the horizon — the project growing over time.', delay: '.5s' },
              ] as const).map(a => (
                <div key={a.k} style={{ position: 'absolute', top: a.top, left: a.left, right: a.right, maxWidth: a.mw, animation: `tg-rise ${a.delay} var(--ease-out) both` }}>
                  <div style={{ fontFamily: 'var(--font-mono)', fontSize: '10px', letterSpacing: '.14em', color: a.eb }}>{a.k}</div>
                  <div style={{ fontSize: '11px', color: 'var(--text-200)', lineHeight: 1.45, marginTop: '2px' }}>{a.d}</div>
                </div>
              ))}
            </div>
          )}
        </div>

        {/* SIDE PANELS */}
        <div style={{ borderLeft: '1px solid var(--line-default)', background: 'rgba(13,11,26,0.55)', display: 'flex', flexDirection: 'column', minHeight: 0 }}>
          {/* metrics strip */}
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3,1fr)', borderBottom: '1px solid var(--line-default)' }}>
            <div style={{ padding: '16px 18px', borderRight: '1px solid var(--line-soft)' }}>
              <div style={{ fontFamily: 'var(--font-mono)', fontSize: '22px', color: 'var(--text-100)' }}>{engine.done}<span style={{ color: 'var(--text-500)', fontSize: '14px' }}>/{engine.issues.length}</span></div>
              <div style={{ fontFamily: 'var(--font-mono)', fontSize: '9px', letterSpacing: '.12em', color: 'var(--text-500)', marginTop: '3px' }}>LEAVES</div>
            </div>
            <div style={{ padding: '16px 18px', borderRight: '1px solid var(--line-soft)' }}>
              <div style={{ fontFamily: 'var(--font-mono)', fontSize: '22px', color: 'var(--flux-green-soft)' }}>{engine.shipped}</div>
              <div style={{ fontFamily: 'var(--font-mono)', fontSize: '9px', letterSpacing: '.12em', color: 'var(--text-500)', marginTop: '3px' }}>SPECS SHIPPED</div>
            </div>
            <div style={{ padding: '16px 18px' }}>
              <div style={{ fontFamily: 'var(--font-mono)', fontSize: '22px', color: 'var(--flux-green-soft)' }}>$0.00</div>
              <div style={{ fontFamily: 'var(--font-mono)', fontSize: '9px', letterSpacing: '.12em', color: 'var(--text-500)', marginTop: '3px' }}>SPEND</div>
            </div>
          </div>

          {/* work-items list (scrolls) */}
          <div style={{ flex: 1, minHeight: 0, display: 'flex', flexDirection: 'column', borderBottom: '1px solid var(--line-default)' }}>
            <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '12px 18px 8px' }}>
              <span style={{ fontFamily: 'var(--font-mono)', fontSize: '10px', letterSpacing: '.16em', color: 'var(--text-400)' }}>WORK ITEMS — {engine.specName}</span>
              <span style={{ fontFamily: 'var(--font-mono)', fontSize: '10px', color: 'var(--text-500)' }}>plan·gen·test·rev·commit</span>
            </div>
            <div className="hf-scroll" style={{ flex: 1, overflowY: 'auto', padding: '0 12px 12px', display: 'flex', flexDirection: 'column', gap: '6px' }}>
              {engine.issues.map(it => {
                const pill = pillFor(it);
                const dim = it.status === 'queued';
                return (
                  <div key={it.id} style={{ display: 'flex', alignItems: 'center', gap: '10px', padding: '9px 11px', borderRadius: 'var(--radius-sm)', border: `1px solid ${it.status === 'failed' ? 'rgba(244,63,94,0.35)' : 'var(--line-soft)'}`, background: dim ? 'transparent' : 'rgba(34,26,64,0.4)' }}>
                    <span style={{ width: '7px', height: '7px', borderRadius: '50%', flex: 'none', background: TYPE_DOT[it.type], boxShadow: `0 0 7px ${TYPE_DOT[it.type]}` }} />
                    <div style={{ minWidth: 0, flex: 1 }}>
                      <div style={{ fontFamily: 'var(--font-mono)', fontSize: '11px', color: 'var(--text-100)', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>
                        <span style={{ color: 'var(--text-500)' }}>#{it.id}</span> {it.title}
                      </div>
                      <div style={{ display: 'flex', gap: '4px', marginTop: '5px' }}>
                        {STAGES.map((_, di) => {
                          let col = 'rgba(255,255,255,0.08)';
                          if (it.status === 'done' || di < it.stage) col = '#10B981';
                          else if (di === it.stage && it.status === 'failed') col = '#F43F5E';
                          else if (di === it.stage && it.status === 'building') col = '#F59E0B';
                          return <span key={di} style={{ width: '16px', height: '3px', borderRadius: '2px', background: col }} />;
                        })}
                      </div>
                    </div>
                    <StatusPill state={pill.state} label={pill.label} pulse={it.status === 'building'} />
                  </div>
                );
              })}
            </div>
          </div>

          {/* build log (190px, scrolls) */}
          <div style={{ height: '190px', display: 'flex', flexDirection: 'column' }}>
            <div style={{ display: 'flex', alignItems: 'center', gap: '8px', padding: '10px 18px 6px' }}>
              <span style={{ width: '7px', height: '7px', borderRadius: '50%', background: 'var(--flux-green)', boxShadow: 'var(--glow-green)' }} />
              <span style={{ fontFamily: 'var(--font-mono)', fontSize: '10px', letterSpacing: '.16em', color: 'var(--text-400)' }}>BUILD LOG</span>
            </div>
            <div className="hf-scroll" style={{ flex: 1, overflowY: 'auto', padding: '2px 18px 12px', display: 'flex', flexDirection: 'column', gap: '4px', fontFamily: 'var(--font-mono)', fontSize: '11px', lineHeight: 1.5 }}>
              {logs.map((ln, i) => (
                <div key={i}><span style={{ color: 'var(--text-500)' }}>{ln.time}</span> <span style={{ color: ln.tagColor }}>{ln.tag}</span> <span style={{ color: 'var(--text-200)' }}>{ln.text}</span></div>
              ))}
            </div>
          </div>
        </div>
      </div>

      {/* ===== SPEC SHEET (below the fold — the page scrolls to it) ===== */}
      <section style={{ borderTop: '1px solid var(--line-default)', padding: '34px 30px 60px', background: 'linear-gradient(180deg,rgba(13,11,26,0.5),rgba(15,26,24,0.4))' }}>
        <div style={{ display: 'flex', alignItems: 'baseline', gap: '12px', marginBottom: '6px' }}>
          <span style={{ fontFamily: 'var(--font-mono)', fontSize: '11px', letterSpacing: '.18em', textTransform: 'uppercase', color: 'var(--flux-green-soft)' }}>Implementation spec</span>
          <span style={{ fontFamily: 'var(--font-mono)', fontSize: '11px', color: 'var(--text-500)' }}>harmony · growth experience v1</span>
        </div>
        <h2 style={{ fontFamily: 'var(--font-sans)', fontWeight: 600, fontSize: '26px', color: 'var(--text-100)', margin: '0 0 26px' }}>Objects, pipeline &amp; motion</h2>

        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit,minmax(230px,1fr))', gap: '16px', marginBottom: '26px' }}>
          {([
            { dot: '#10B981', glow: true, name: 'Leaf', tag: '= issue', body: 'A single work item. Renders once its issue enters building, matures to green on commit. State drives fill: amber/rose/green. Radius 5, halo 11, breathe on active.' },
            { dot: '#7C3AED', glow: true, name: 'Bough & sap', tag: '= work cluster', body: 'A structural branch holding a leaf cluster. Grows via stroke-dashoffset (pathLength=1) when its first issue starts. Violet sap pulses travel it while work is active.' },
            { dot: '#4a3d63', glow: false, name: 'Tree', tag: '= branch', body: 'One git branch. Generated deterministically from the spec (seeded), sized to hold exactly N leaves. Complete + committed → merged → joins the forest.' },
            { dot: '#34D399', glow: true, name: 'Forest', tag: '= spec / epic', body: "The horizon of shipped specs. Persists in localStorage across sessions — the project's history, growing with parallax and fog." },
          ] as const).map(o => (
            <div key={o.name} style={objectCard}>
              <div style={{ display: 'flex', alignItems: 'center', gap: '8px', marginBottom: '8px' }}>
                <span style={{ width: '10px', height: '10px', borderRadius: '50%', background: o.dot, boxShadow: o.glow ? `0 0 8px ${o.dot}` : undefined, border: o.glow ? undefined : '1px solid var(--line-strong)' }} />
                <span style={{ fontWeight: 600, fontSize: '14px', color: 'var(--text-100)' }}>{o.name}</span>
                <span style={{ marginLeft: 'auto', fontFamily: 'var(--font-mono)', fontSize: '10px', color: 'var(--text-500)' }}>{o.tag}</span>
              </div>
              <div style={{ fontSize: '12.5px', lineHeight: 1.55, color: 'var(--text-300)' }}>{o.body}</div>
            </div>
          ))}
        </div>

        <div style={{ display: 'grid', gridTemplateColumns: '1.4fr 1fr', gap: '16px' }}>
          <div style={{ ...objectCard, padding: '20px' }}>
            <div style={{ fontFamily: 'var(--font-mono)', fontSize: '10px', letterSpacing: '.16em', color: 'var(--text-400)', marginBottom: '16px' }}>PIPELINE — each issue, in order</div>
            <div style={{ display: 'flex', alignItems: 'center', gap: '6px', flexWrap: 'wrap' }}>
              {([
                { t: '1 · plan', c: 'var(--flux-blue-soft)', b: 'rgba(59,130,246,0.3)' },
                { t: '2 · generate', c: 'var(--violet-300)', b: 'var(--line-strong)' },
                { t: '3 · test', c: 'var(--flux-amber)', b: 'rgba(245,158,11,0.34)' },
                { t: '4 · review', c: 'var(--violet-300)', b: 'var(--line-strong)' },
                { t: '5 · commit', c: 'var(--flux-green-soft)', b: 'rgba(16,185,129,0.32)' },
              ] as const).map((chip, i, arr) => (
                <span key={chip.t} style={{ display: 'contents' }}>
                  <span style={{ fontFamily: 'var(--font-mono)', fontSize: '12px', color: chip.c, border: `1px solid ${chip.b}`, borderRadius: 'var(--radius-pill)', padding: '6px 13px' }}>{chip.t}</span>
                  {i < arr.length - 1 && <span style={{ color: 'var(--text-500)' }}>→</span>}
                </span>
              ))}
            </div>
            <div style={{ fontSize: '12.5px', lineHeight: 1.55, color: 'var(--text-300)', marginTop: '16px' }}>Up to 4 issues run concurrently. A test or review may fail (rose), pause, then retry (amber) before continuing. Only a clean commit lights the leaf. Rate slider scales all durations; the loop respects prefers-reduced-motion.</div>
          </div>
          <div style={{ ...objectCard, padding: '20px' }}>
            <div style={{ fontFamily: 'var(--font-mono)', fontSize: '10px', letterSpacing: '.16em', color: 'var(--text-400)', marginBottom: '14px' }}>SEMANTIC COLOR &amp; MOTION</div>
            <div style={{ display: 'flex', flexDirection: 'column', gap: '9px' }}>
              {([
                ['#10B981', 'Green — done / grown / free'],
                ['#7C3AED', 'Violet — sap, in-flight energy'],
                ['#F59E0B', 'Amber — building / retrying'],
                ['#F43F5E', 'Rose — failed step'],
              ] as const).map(([sw, txt]) => (
                <div key={txt} style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
                  <span style={{ width: '20px', height: '20px', borderRadius: '5px', background: sw, flex: 'none' }} />
                  <span style={{ fontSize: '12px', color: 'var(--text-200)' }}>{txt}</span>
                </div>
              ))}
            </div>
            <div style={{ fontSize: '12px', lineHeight: 1.5, color: 'var(--text-400)', marginTop: '14px', borderTop: '1px solid var(--line-soft)', paddingTop: '12px' }}>Easing cubic-bezier(.16,1,.3,1) · leaf pop 600ms · bough grow 700ms · sap loop 2.6–3.6s · join 1.8s.</div>
          </div>
        </div>
      </section>
    </div>
  );
}
