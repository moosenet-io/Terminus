// CGUI-11 (TERM #534): the Harmony Forest Build simulation — a framework-agnostic port of the
// state machine + procedural tree generator from the design handoff ("Harmony Forest Build.dc.html").
// The React panel (HarmonyForestPanel.tsx) owns rendering + the tick interval; this module owns the
// pure(ish) build-orchestrator sim so it can be unit-tested without a DOM. `buildTree` is fully
// deterministic (seeded mulberry32 — no Math.random), so the same spec name+size always grows the
// exact same tree; only per-issue speed/sha jitter and the fail-roll use Math.random, none of which
// affect tree geometry. localStorage/setTimeout are accessed through guarded adapters so the engine
// is safe to instantiate in a node test environment.

export const STAGES = ['plan', 'generate', 'test', 'review', 'commit'] as const;
export const CONC = 4;
export const TICK_MS = 110;
export const LS_KEY = 'harmony.forest.v1';
const FOREST_CAP = 80;
const LOG_CAP = 60;

const VERB = ['add', 'wire', 'guard', 'cache', 'stream', 'retry', 'batch', 'trace', 'seal', 'prune', 'emit', 'mount', 'sync', 'scope', 'patch', 'hoist'];
const NOUN = ['auth-guard', 'token-vault', 'rate-limiter', 'session-store', 'webhook', 'scheduler', 'memory-tier', 'router', 'parser', 'indexer', 'circuit-breaker', 'briefing', 'notifier', 'proxy', 'loader', 'worker', 'migration', 'resolver'];
const TYPES = ['feature', 'fix', 'test', 'refactor', 'docs'] as const;

export type IssueType = (typeof TYPES)[number];
export type IssueStatus = 'queued' | 'building' | 'failed' | 'done';
export type Phase = 'idle' | 'building' | 'commit' | 'joining' | 'shipped';

/** Type-dot colors are viz-canonical semantic paints (kept as literals, per the CGUI-11 brief). */
export const TYPE_DOT: Record<IssueType, string> = {
  feature: '#7C3AED',
  fix: '#F59E0B',
  test: '#3B82F6',
  refactor: '#A855F7',
  docs: '#10B981',
};

export interface SpecPreset { name: string; issues: number; }
export const SPECS: SpecPreset[] = [
  { name: 'auth-service', issues: 12 },
  { name: 'notify-hub', issues: 16 },
  { name: 'billing-engine', issues: 24 },
  { name: 'platform-core', issues: 40 },
];

// ── seeded rng + geometry helpers (verbatim semantics from the prototype) ──────────────────

export function mulberry32(a: number): () => number {
  return function () {
    a |= 0; a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
export function hashStr(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i); h = Math.imul(h, 16777619); }
  return h >>> 0;
}
export function hex6(): string { return Math.floor(Math.random() * 0xffffff).toString(16).padStart(6, '0'); }
export const clamp = (v: number, a: number, b: number): number => Math.max(a, Math.min(b, v));

interface Pt { x: number; y: number; }
function quad(p0: Pt, c: Pt, p1: Pt, t: number): Pt {
  const m = 1 - t;
  return { x: m * m * p0.x + 2 * m * t * c.x + t * t * p1.x, y: m * m * p0.y + 2 * m * t * c.y + t * t * p1.y };
}
/** SVG leaf blade path, scaled by `s`. */
export function leafPath(s: number): string {
  return `M0 ${(0.6 * s).toFixed(1)} C ${(-0.64 * s).toFixed(1)} ${(0.12 * s).toFixed(1)} ${(-0.44 * s).toFixed(1)} ${(-0.98 * s).toFixed(1)} 0 ${(-s).toFixed(1)} C ${(0.44 * s).toFixed(1)} ${(-0.98 * s).toFixed(1)} ${(0.64 * s).toFixed(1)} ${(0.12 * s).toFixed(1)} 0 ${(0.6 * s).toFixed(1)} Z`;
}

// ── domain types ───────────────────────────────────────────────────────────────────────────

export interface FillLeaf { x: number; y: number; ang: number; sz: number; g: boolean; }
export interface Bough { id: string; d: string; dur: string; fill: FillLeaf[]; }
export interface Leaf { id: string; x: number; y: number; boughId: string; }
export interface Tree { trunk: { d: string }; boughs: Bough[]; leaves: Leaf[]; order: Leaf[]; }

export interface Issue {
  id: number; title: string; type: IssueType;
  stage: number; p: number; status: IssueStatus;
  spd: number; failT: number; retried: boolean;
  leaf: Leaf; boughId: string; sha: string;
}
export interface LogLine { time: string; tag: string; tagColor: string; text: string; }
export interface ForestTree { s: number; x: number; sc: number; y: number; n: string; isNew: boolean; }

// ── injectable side-effect adapters (guarded for node test envs) ────────────────────────────

interface StorageAdapter { get(k: string): string | null; set(k: string, v: string): void; }
const defaultStorage: StorageAdapter = {
  get(k) { try { return typeof localStorage !== 'undefined' ? localStorage.getItem(k) : null; } catch { return null; } },
  set(k, v) { try { if (typeof localStorage !== 'undefined') localStorage.setItem(k, v); } catch { /* no-op */ } },
};

export interface EngineOptions {
  onChange?: () => void;
  storage?: StorageAdapter;
  schedule?: (fn: () => void, ms: number) => ReturnType<typeof setTimeout>;
  clearScheduled?: (h: ReturnType<typeof setTimeout>) => void;
}

// ── the engine ───────────────────────────────────────────────────────────────────────────

export class ForestEngine {
  speed = 1;
  annotate = false;
  phase: Phase = 'idle';
  running = false;
  specName = 'auth-service';
  size = 12;
  issues: Issue[] = [];
  logsArr: LogLine[] = [];
  forest: ForestTree[] = [];
  shipped = 0;
  revealed: Record<string, boolean> = {};
  trunkGrown = false;
  done = 0;
  inFlight = 0;
  ticks = 0;
  tree: Tree | null = null;

  private onChange?: () => void;
  private storage: StorageAdapter;
  private schedule: (fn: () => void, ms: number) => ReturnType<typeof setTimeout>;
  private clearScheduled: (h: ReturnType<typeof setTimeout>) => void;
  private timers: ReturnType<typeof setTimeout>[] = [];

  constructor(opts: EngineOptions = {}) {
    this.onChange = opts.onChange;
    this.storage = opts.storage ?? defaultStorage;
    this.schedule = opts.schedule ?? ((fn, ms) => setTimeout(fn, ms));
    this.clearScheduled = opts.clearScheduled ?? (h => clearTimeout(h));
  }

  setOnChange(cb: () => void): void { this.onChange = cb; }
  private bump(): void { this.onChange?.(); }

  destroy(): void { this.timers.forEach(t => this.clearScheduled(t)); this.timers = []; }

  loadForest(): void {
    const raw = this.storage.get(LS_KEY);
    if (!raw) return;
    try {
      const f = JSON.parse(raw);
      if (Array.isArray(f)) { this.forest = f as ForestTree[]; this.shipped = f.length; }
    } catch { /* corrupt payload — start fresh */ }
  }
  private saveForest(): void { this.storage.set(LS_KEY, JSON.stringify(this.forest.slice(-FOREST_CAP))); }

  buildTree(name: string, size: number): Tree {
    const rng = mulberry32(hashStr(name) + size * 97);
    const trunkTop = { x: 500 + (rng() - 0.5) * 28, y: 468 - rng() * 18 };
    const trunk = { d: `M500 668 Q ${(500 + (rng() - 0.5) * 22).toFixed(1)} 585 ${trunkTop.x.toFixed(1)} ${trunkTop.y.toFixed(1)}` };
    const boughCount = clamp(Math.round(3 + size / 7), 3, 7);
    const per = Math.floor(size / boughCount);
    const extra = size - per * boughCount;
    const boughs: Bough[] = [];
    const leaves: Leaf[] = [];
    let idx = 0;
    for (let i = 0; i < boughCount; i++) {
      const t = boughCount === 1 ? 0.5 : i / (boughCount - 1);
      const ang = (-76 + 152 * t + (rng() - 0.5) * 20) * Math.PI / 180;
      const len = 150 + rng() * 66 - Math.abs(t - 0.5) * 40;
      const dx = Math.sin(ang), dy = -Math.cos(ang);
      const start = { x: trunkTop.x + (rng() - 0.5) * 14, y: trunkTop.y + (i % 2 ? 10 : -2) - rng() * 6 };
      const tip = { x: start.x + dx * len, y: start.y + dy * len };
      const px = -dy, py = dx;
      const ctrl = { x: (start.x + tip.x) / 2 + px * (rng() - 0.5) * 80, y: (start.y + tip.y) / 2 + py * (rng() - 0.5) * 80 };
      const bid = 'b' + i;
      const fill: FillLeaf[] = [];
      const fc = Math.round(9 + len / 13);
      for (let k = 0; k < fc; k++) {
        const ft = Math.min(0.98, 0.2 + (k / (fc - 1)) * 0.78 + (rng() - 0.5) * 0.05);
        const P = quad(start, ctrl, tip, ft);
        const side = k % 2 ? 1 : -1;
        const foff = (6 + rng() * 30) * side;
        const fx = P.x + px * foff + (rng() - 0.5) * 8, fy = P.y + py * foff - rng() * 8;
        fill.push({ x: +fx.toFixed(1), y: +fy.toFixed(1), ang: +((Math.atan2(fy - P.y, fx - P.x) * 180 / Math.PI) + 90 + (rng() - 0.5) * 44).toFixed(1), sz: +(6.5 + rng() * 4.5).toFixed(1), g: rng() > 0.5 });
      }
      boughs.push({ id: bid, d: `M${start.x.toFixed(1)} ${start.y.toFixed(1)} Q ${ctrl.x.toFixed(1)} ${ctrl.y.toFixed(1)} ${tip.x.toFixed(1)} ${tip.y.toFixed(1)}`, dur: (2.6 + rng() * 1.1).toFixed(2), fill });
      const cnt = per + (i < extra ? 1 : 0);
      for (let j = 0; j < cnt; j++) {
        let along = 0.32 + 0.64 * (cnt <= 1 ? 1 : j / (cnt - 1)) + rng() * 0.05;
        along = Math.min(0.99, along);
        const P = quad(start, ctrl, tip, along);
        const off = (j % 2 ? 1 : -1) * (12 + rng() * 40);
        leaves.push({ id: 'l' + idx, x: +(P.x + px * off + (rng() - 0.5) * 12).toFixed(1), y: +(P.y + py * off - rng() * 16).toFixed(1), boughId: bid });
        idx++;
      }
    }
    const order = [...leaves].sort((a, b) => (b.y - a.y) || (Math.abs(a.x - 500) - Math.abs(b.x - 500)));
    return { trunk, boughs, leaves, order };
  }

  loadSpec(name: string, size: number): void {
    this.timers.forEach(t => this.clearScheduled(t)); this.timers = [];
    this.specName = name; this.size = size;
    this.tree = this.buildTree(name, size);
    this.issues = this.tree.order.map((leaf, i) => ({
      id: 100 + i,
      title: `${VERB[i % VERB.length]}-${NOUN[(i * 7 + 3) % NOUN.length]}`,
      type: TYPES[(i * 5 + 2) % TYPES.length],
      stage: 0, p: 0, status: 'queued', spd: 0.05 + Math.random() * 0.06,
      failT: 0, retried: false, leaf, boughId: leaf.boughId, sha: hex6(),
    }));
    this.revealed = {}; this.trunkGrown = false; this.done = 0; this.inFlight = 0; this.ticks = 0;
    this.phase = 'idle'; this.running = false;
    this.logsArr = [{ time: this.clock(), tag: '[..]', tagColor: 'var(--flux-blue-soft)', text: `spec ${name} loaded — ${size} issues planned` }];
    this.bump();
    this.schedule(() => { this.trunkGrown = true; this.bump(); }, 60);
  }

  clock(): string {
    const s = Math.floor(this.ticks * TICK_MS / 1000);
    return `${String(Math.floor(s / 60)).padStart(2, '0')}:${String(s % 60).padStart(2, '0')}`;
  }
  private log(tag: string, color: string, text: string): void {
    this.logsArr.push({ time: this.clock(), tag, tagColor: color, text });
    if (this.logsArr.length > LOG_CAP) this.logsArr.shift();
  }

  start(): void {
    if (this.phase === 'idle' || this.phase === 'shipped') {
      if (this.done > 0 || this.phase === 'shipped') this.loadSpec(this.specName, this.size);
      this.phase = 'building';
    }
    this.running = true;
    this.bump();
  }

  /** One simulation tick. Advances every building issue's pipeline, promotes queued issues into
   *  the up-to-CONC concurrency window, rolls the ~13% test/review failure (one retry only), and
   *  fires `finish()` when every issue has committed. */
  step(): void {
    this.ticks++;
    for (const it of this.issues) {
      if (it.status === 'failed') {
        // Drain the retry countdown at the RATE factor too (not a flat 1/tick), so RATE governs
        // the failure→retry wall-clock exactly like it governs pipeline progress (§2.5).
        it.failT -= this.speed;
        if (it.failT <= 0) { it.status = 'building'; it.retried = true; this.log('[..]', 'var(--flux-amber)', `#${it.id} ${STAGES[it.stage]} retry`); }
        continue;
      }
      if (it.status !== 'building') continue;
      it.p += it.spd * this.speed;
      if (it.p >= 1) {
        it.p = 0;
        if (it.stage >= 4) {
          it.status = 'done'; this.done++; this.inFlight--;
          this.log('[ok]', 'var(--flux-green-soft)', `#${it.id} committed ${it.sha}`);
        } else {
          it.stage++;
          if ((it.stage === 2 || it.stage === 3) && !it.retried && Math.random() < 0.13) {
            it.status = 'failed'; it.failT = 4 + Math.floor(Math.random() * 6);
            this.log('[!!]', '#FB7185', `#${it.id} ${STAGES[it.stage]} failed`);
          }
        }
      }
    }
    let guard = 0;
    while (this.inFlight < CONC && guard < CONC) {
      const nx = this.issues.find(i => i.status === 'queued');
      if (!nx) break;
      nx.status = 'building'; this.inFlight++; this.revealed[nx.boughId] = true;
      this.log('[..]', 'var(--flux-blue-soft)', `#${nx.id} plan started`); guard++;
    }
    if (this.done === this.issues.length && this.issues.length > 0 && this.phase === 'building') this.finish();
    this.bump();
  }

  private finish(): void {
    this.phase = 'commit'; this.running = false;
    this.log('[ok]', 'var(--flux-green-soft)', `branch ${this.specName} — all issues merged`); this.bump();
    // Scale the merge→join→reload sequence by RATE too (§2.5: RATE governs the WHOLE animation).
    // Divide because a higher RATE means faster = shorter wall-clock, mirroring how RATE speeds
    // per-stage progress. `speed` is clamped to [0.5, 2.5] by the slider, so never zero.
    this.timers.push(this.schedule(() => { this.phase = 'joining'; this.bump(); }, 1400 / this.speed));
    this.timers.push(this.schedule(() => { this.joinForest(); }, 3300 / this.speed));
  }
  private joinForest(): void {
    const rng = mulberry32(hashStr(this.specName) + this.shipped * 131 + this.forest.length);
    this.forest.push({ s: Math.floor(rng() * 99999), x: 8 + rng() * 84, sc: 0.55 + rng() * 0.5, y: rng(), n: this.specName, isNew: true });
    this.shipped = this.forest.length; this.saveForest();
    this.phase = 'shipped';
    this.log('[ok]', 'var(--flux-green-soft)', `forest +1 — ${this.shipped} specs shipped`); this.bump();
    this.timers.push(this.schedule(() => { this.forest.forEach(f => (f.isNew = false)); this.loadSpec(this.specName, this.size); }, 1600 / this.speed));
  }

  // ── control surface (called by the panel's header buttons/sliders) ──
  selectSpec(name: string, size: number): void { this.loadSpec(name, size); }
  setSize(size: number): void { this.loadSpec('custom', size); }
  setSpeed(speed: number): void { this.speed = speed; }
  grow(): void { if (this.phase !== 'building') this.start(); }
  togglePause(): void { this.running = !this.running; this.bump(); }
  reset(): void { this.loadSpec(this.specName, this.size); }
  toggleAnnotate(): void { this.annotate = !this.annotate; this.bump(); }
  tickIfRunning(): void { if (this.running && this.phase === 'building') this.step(); }
}
