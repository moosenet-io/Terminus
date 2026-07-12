// ROUTE-03: Preset type definitions mirroring the Rust PresetConfig model.
// RENAME-04: Added OperatingMode types alongside legacy preset types.

export interface PresetInfo {
  notch: number;       // 1-10
  name: string;
  description: string;
  workers: number;
  costPerDay: string;
}

export interface InferenceMixState {
  preset: number;      // 1-10
  limited: boolean;
}

// ── RENAME-04: Operating mode types ──────────────────────────────────────────

// GROW-08 added LocalEnhanced (3 slots: 2 GPU + 1 CPU) and CloudPlus (9-12 slots).
export type ModeId = 'local' | 'local_enhanced' | 'assisted' | 'hybrid' | 'cloud_plus' | 'cloud';

export interface ModeInfo {
  id: ModeId;
  label: string;
  cost: string;
  desc: string;
  /** Representative notch for backward compat */
  notch: number;
}

export const MODES: ModeInfo[] = [
  { id: 'local',          label: 'Local',          cost: '$0/day',      desc: 'GPU + CPU only. No cloud costs.',                  notch: 1  },
  { id: 'local_enhanced', label: 'Local Enhanced', cost: '$0/day',      desc: '2 GPU slots + 1 CPU. More local parallelism.',     notch: 2  },
  { id: 'assisted',       label: 'Assisted',       cost: '~$3/day',     desc: 'Local code + cloud review.',                       notch: 3  },
  { id: 'hybrid',         label: 'Hybrid',         cost: '~$10/day',    desc: 'Local + cloud coders in parallel.',                notch: 6  },
  { id: 'cloud_plus',     label: 'Cloud+',         cost: '~$20-40/day', desc: '9-12 slots: standard + quick cloud in parallel.',  notch: 8  },
  { id: 'cloud',          label: 'Cloud',          cost: '~$30+/day',   desc: 'All providers, max parallelism.',                  notch: 10 },
];

export interface ModeState {
  mode: ModeId;
  limited: boolean;
  updated_at?: string;
}

// ── Legacy preset table (kept for backward compat) ────────────────────────────

export const PRESETS: PresetInfo[] = [
  { notch: 1,  name: 'Local baseline',       description: 'GPU + CPU only, cloud off',           workers: 2,  costPerDay: '$0/day'    },
  { notch: 2,  name: 'Local + cloud review', description: 'Local execute, cloud review gate',     workers: 3,  costPerDay: '~$0.50/day' },
  { notch: 3,  name: 'Local + cloud support',description: 'Local execute, cloud enrich + review', workers: 4,  costPerDay: '~$1/day'   },
  { notch: 4,  name: 'One cloud coder',       description: 'GPU + CPU + 1 cloud exec worker',     workers: 3,  costPerDay: '~$3/day'   },
  { notch: 5,  name: 'Balanced',              description: 'Local + sonnet + codex',              workers: 4,  costPerDay: '~$5/day'   },
  { notch: 6,  name: 'Wide parallel',         description: 'All 5 providers, one worker each',    workers: 5,  costPerDay: '~$8/day'   },
  { notch: 7,  name: 'Cloud priority',        description: 'Cloud handles complex tasks first',   workers: 5,  costPerDay: '~$12/day'  },
  { notch: 8,  name: 'Blitz swarm',           description: '8 workers, quick tier, high throughput', workers: 8, costPerDay: '~$10/day' },
  { notch: 9,  name: 'Full sprint',           description: '8 workers, all providers, mixed tiers', workers: 8, costPerDay: '~$18/day' },
  { notch: 10, name: 'Full swarm',            description: 'VRAM-limited GPU + all cloud providers', workers: 12, costPerDay: '~$30+/day' },
];
