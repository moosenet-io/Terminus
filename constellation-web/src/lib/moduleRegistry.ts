// CONST-04: Panels register themselves here instead of the shell hardcoding a page table.
// The shell (App.tsx / Sidebar) only ever renders what `getAvailablePanels()` /
// `getPanelsBySystem()` return — an unregistered or unavailable capability simply doesn't
// render. No crash, no "coming soon" placeholder needed at the shell level.
import type { ComponentType } from 'react';

/** Nav grouping — matches CONST-01 §3: Harmony / Chord / Lumina / Terminus / Providers / Status. */
export type SystemGroup = 'Harmony' | 'Chord' | 'Lumina' | 'Terminus' | 'Providers' | 'Status';

export interface PanelDescriptor {
  /** Stable unique id, e.g. "terminus.config". */
  id: string;
  system: SystemGroup;
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

const SYSTEM_ORDER: SystemGroup[] = ['Harmony', 'Chord', 'Lumina', 'Terminus', 'Providers', 'Status'];

/** Available panels grouped by nav system, in the fixed CONST-01 display order. */
export function getPanelsBySystem(): { system: SystemGroup; panels: PanelDescriptor[] }[] {
  const available = getAvailablePanels();
  return SYSTEM_ORDER.map(system => ({
    system,
    panels: available.filter(p => p.system === system),
  })).filter(group => group.panels.length > 0);
}

/** Test/dev helper — not used by the shell at runtime. */
export function clearRegistry(): void {
  registry.clear();
}
