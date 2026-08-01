// CONST-04: Panels register themselves here instead of the shell hardcoding a page table.
// CONST-16 layers a ModuleDescriptor registry ABOVE this — a module groups panels, binds
// availability to a health source, and owns a global-bar tab (§1.3 of the CONST-GUI spec).
// PanelDescriptor itself is UNCHANGED: every already-registered panel keeps working, it just
// now points `system` at a module id instead of the old capitalized nav-group label.
import type { ComponentType } from 'react';
import type { HealthStatus } from './aggregationClient';

// ── Module registry (CONST-16, §1.3) ────────────────────────────────────────

/**
 * Stable module ids — one per fleet system's presence in the GUI. Widened (from the CONST-04
 * `SystemGroup`) to include `muse`/`models`/`mint` ahead of their own build items (CONST-19..24)
 * landing panels against them — the id is stable from day one even though no module/panel is
 * registered for them yet in this item.
 */
/** MACT-04 (MUSE-124): `maestro` is a Muse subsystem — the live-activity/playback-control
 *  surface, same relationship Models/MINT have to Terminus (its own registered module with
 *  panels/routes/health, nested under the Muse core tab rather than a sibling top-level tab).
 *  `healthSystem: 'muse'` because in H1 the live pane is still muse-derived data (see
 *  `aggregationClient.ts`'s `LiveSession.source` doc); it stays bound to `muse` even once H2's
 *  real Maestro service lands, since Muse's own health remains the meaningful gate either way. */
export type ModuleId = 'harmony' | 'chord' | 'lumina' | 'muse' | 'terminus' | 'models' | 'mint' | 'maestro';

export interface ModuleDescriptor {
  /** Stable id and data namespace (see `ModuleId`). For proxied systems this doubles as the
   *  aggregation `SystemId`; for terminus-backed modules (models/mint/terminus) the data
   *  source is the terminus namespace. */
  id: ModuleId;
  /** Global-bar tab / rail group title, e.g. "Model Library". */
  title: string;
  icon: string;
  /** Which `/api/health` system-entry gates this module's availability; 'terminus' modules
   *  (models/mint/terminus) bind to the always-available terminus self entry. */
  healthSystem: 'harmony' | 'chord' | 'lumina' | 'muse' | 'terminus';
  /** Fixed global-bar order (stable across health flaps — modules never reorder at runtime). */
  order: number;
  /** Minimum role that may see this module at all (default 'viewer'); mutating controls inside
   *  panels additionally gate on 'operator' (CONST-27, not enforced here). */
  minRole?: 'viewer' | 'operator';
}

const moduleRegistry = new Map<ModuleId, ModuleDescriptor>();

/** Register (or replace) a module descriptor. Call once per module, at import time
 *  (`registerPanels.ts`), same convention as `registerPanel`. */
export function registerModule(m: ModuleDescriptor): void {
  moduleRegistry.set(m.id, m);
}

/** All registered modules regardless of availability (diagnostic use only), in `order`. */
export function getAllModules(): ModuleDescriptor[] {
  return Array.from(moduleRegistry.values()).sort((a, b) => a.order - b.order);
}

/**
 * Modules the shell may show as a global-bar tab / render a card for: registered AND their
 * `healthSystem` entry in the given health snapshot reports `available: true`. Callers own any
 * stale-while-degrading grace (App.tsx applies the 2-cycle grace to the health snapshot before
 * calling this) — this function itself is a pure filter over whatever health it is given.
 */
export function getAvailableModules(health: HealthStatus[]): ModuleDescriptor[] {
  const bySystem = new Map<string, HealthStatus>(health.map(h => [h.system, h]));
  return getAllModules().filter(m => bySystem.get(m.healthSystem)?.available === true);
}

/** Test/dev helper — not used by the shell at runtime. */
export function clearModuleRegistry(): void {
  moduleRegistry.clear();
}

// ── Legacy nav-group mapping (CONST-04 → CONST-16 migration) ────────────────

/** @deprecated The pre-CONST-16 nav grouping. Kept ONLY for `legacySystemGroupToModuleId` (the
 *  mechanical map §1.3 rule 1 describes) and its unit test — no panel registration should use
 *  these capitalized labels going forward; use a lowercase `ModuleId` directly. */
export type SystemGroup = 'Harmony' | 'Chord' | 'Lumina' | 'Terminus' | 'Providers' | 'Status';

const LEGACY_SYSTEM_GROUP_MAP: Record<SystemGroup, ModuleId> = {
  Harmony: 'harmony',
  Chord: 'chord',
  Lumina: 'lumina',
  Terminus: 'terminus',
  // 'Status' as a top-level group dissolves into Overview/Harmony — its two panels
  // (Analytics, Engine Diagram) render Harmony/Chord build-pipeline data (spec §5.1).
  Status: 'harmony',
  // 'Providers' was a reserved-but-unused CONST-01 nav slot; it re-homes under terminus.
  Providers: 'terminus',
};

/** The mechanical map from a legacy `SystemGroup` label to its canonical `ModuleId` (§1.3 rule 1). */
export function legacySystemGroupToModuleId(group: SystemGroup): ModuleId {
  return LEGACY_SYSTEM_GROUP_MAP[group];
}

// ── Panel registry (CONST-04, unchanged contract) ───────────────────────────

export interface PanelDescriptor {
  /** Stable unique id, e.g. "terminus.config". */
  id: string;
  /** The module this panel belongs to. */
  system: ModuleId;
  title: string;
  /** Route path, e.g. "/terminus/config". Must be unique. */
  path: string;
  icon?: string;
  /**
   * Whether this panel's backing capability is actually present. A panel can be *registered*
   * (imported, code exists) but not *available* (its backend hasn't landed yet — e.g. CONST-09
   * waiting on S96). Only available panels are rendered/routed/shown in nav.
   */
  available: boolean;
  component: ComponentType;
  /** CONST-22: true for a panel reachable only via a parameterized/URL-state route rather than
   *  a plain nav destination (e.g. `models.compare` at `/models/compare?m=a&m=b…`, reached via
   *  a Compare action + its own URL-state selection, never a bare rail link). Model DETAIL is
   *  NOT a separate registered panel/route — it's an inline master-detail swap inside
   *  `RosterPanel.tsx` (`ModelDetailView`), so it needs no `hideInRail` entry of its own.
   *  Default false/absent — every other panel keeps rendering in the rail exactly as before. */
  hideInRail?: boolean;
  /** MGUI-18: this panel is a CATALOG surface (a grid of media cards or a wide table), so the
   *  shell canvas caps it at `--content-max-wide` instead of POL-03's `--content-max` reading
   *  measure. Opt-in per panel, deliberately: POL-03's ~1280px cap is right for a column of
   *  prose and charts and wrong for a 1892-tile poster wall on an ultrawide. Default
   *  false/absent — every other panel keeps the standard cap exactly as before. */
  wide?: boolean;
  /** MGUI-20: the panel this one sits UNDER in the rail, by id.
   *
   *  Movies and TV Shows are not siblings of Library, they are what Library CONTAINS. Rendering
   *  them flat produced one combined list plus two segments of the same library beside it —
   *  three nav entries over the same rows, with no signal that two of them partition the third.
   *
   *  Purely presentational: routing, availability and `wide` are unchanged. A child whose
   *  parent is absent or unavailable falls back to rendering flat, so a mis-typed id degrades to
   *  the old layout rather than hiding the panel. */
  parentId?: string;
  /** MGUI-20: this panel is a NAV GROUPING, not a destination — it heads a set of children and
   *  has no list of its own. `path` is still required (the router redirects it to the first
   *  child, so an existing bookmark lands somewhere real rather than 404ing). */
  groupOnly?: boolean;
}

const registry = new Map<string, PanelDescriptor>();

/** Register (or replace) a panel descriptor. Call once per panel module, at import time. */
export function registerPanel(panel: PanelDescriptor): void {
  registry.set(panel.id, panel);
}

/** All registered panels, regardless of availability (diagnostic use only). */
export function getAllPanels(): PanelDescriptor[] {
  return Array.from(registry.values());
}

/** Panels the shell is allowed to render/route/link to. */
export function getAvailablePanels(): PanelDescriptor[] {
  return getAllPanels().filter(p => p.available);
}

/** Available panels belonging to one module, in registration order — feeds the ModuleRail. */
export function getPanelsByModule(moduleId: ModuleId): PanelDescriptor[] {
  return getAvailablePanels().filter(p => p.system === moduleId);
}

/** Is a panel with this id both registered AND available right now? (LGUI-06 review fix:
 *  panels must check this dynamically before redirecting to another panel's route — a
 *  redirect to an unregistered route just bounces off the shell's wildcard Route, which is
 *  worse than not redirecting at all. Self-activates the moment the target panel registers,
 *  with zero change at the call site.) */
export function isPanelAvailable(id: string): boolean {
  return getAvailablePanels().some(p => p.id === id);
}

/** Test/dev helper — not used by the shell at runtime. */
export function clearRegistry(): void {
  registry.clear();
  clearModuleRegistry();
}


/** MGUI-20: rail order — each parent immediately followed by its children, at depth 1.
 *
 * Extracted from `CoreRail` so the rule is testable: the nesting exists to stop Movies and TV
 * Shows reading as two more siblings over the same rows as Library, and "did the child end up
 * under its parent" is exactly the kind of thing that silently regresses when someone reorders
 * a registration block.
 *
 * A child whose parent is ABSENT from the list (unavailable, hidden in rail, or a mis-typed id)
 * is emitted flat rather than dropped — a wrong id degrades to the old layout instead of making
 * the panel unreachable from navigation.
 */
export function railOrder(
  panels: PanelDescriptor[],
): { panel: PanelDescriptor; depth: number }[] {
  const byId = new Map(panels.map(p => [p.id, p]));
  const out: { panel: PanelDescriptor; depth: number }[] = [];
  for (const p of panels) {
    if (p.parentId && byId.has(p.parentId)) continue;
    out.push({ panel: p, depth: 0 });
    for (const child of panels) {
      if (child.parentId === p.id) out.push({ panel: child, depth: 1 });
    }
  }
  return out;
}
