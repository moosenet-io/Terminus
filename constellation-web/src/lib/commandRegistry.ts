// CONST-25 (§3.2): action-source registry for the CommandPalette — a sibling of
// `registerPanel`/`registerModule` in `moduleRegistry.ts`, same convention (register once, at
// import time, from a panel's own module or `registerPanels.ts`; the palette never hardcodes a
// command table). Navigation entries are NOT registered here — the palette derives those
// directly from `getAvailablePanels()` (moduleRegistry.ts), same source GlobalBar already used
// for the pre-CONST-25 MiniPalette. This registry is only for palette-native *actions*.
export type CommandId = string;

export interface CommandDescriptor {
  /** Stable unique id, e.g. "harmony.refresh-health". Duplicate ids are rejected — see
   *  `registerCommand` below — so two panels can never silently clobber each other's entry. */
  id: CommandId;
  /** Palette label, e.g. "Refresh health". */
  title: string;
  /** Optional secondary text shown under the title (what it does / where it applies). */
  subtitle?: string;
  icon?: string;
  /** Minimum role required to run this command. Defaults to 'viewer' (visible/enabled to
   *  everyone). 'operator'-gated commands are hidden (not merely disabled) for a viewer session
   *  — see `getAvailableCommands` — since a hidden entry doesn't leak the existence of a control
   *  a viewer can't use, matching how the rest of the shell treats viewer-gating. */
  minRole?: 'viewer' | 'operator';
  /** Invoked when the user selects this command. May be async; the palette closes optimistically
   *  on invoke and does not await the result (a command failing is the command's own concern —
   *  same fire-and-forget contract as the rest of the shell's action buttons). */
  run: () => void | Promise<void>;
}

const registry = new Map<CommandId, CommandDescriptor>();

/**
 * Register (or attempt to register) a palette command. Duplicate ids are REJECTED — unlike
 * `registerPanel`/`registerModule` (which allow last-registration-wins for hot-reload
 * convenience), a silently-clobbered action command is a much easier bug to hide (two panels
 * both offering "Refresh" that fight over which one's `run` actually fires). Throws
 * synchronously so the mistake surfaces at import time, not the first time a user opens the
 * palette.
 */
export function registerCommand(command: CommandDescriptor): void {
  if (registry.has(command.id)) {
    throw new Error(`registerCommand: duplicate command id "${command.id}" — ids must be unique`);
  }
  registry.set(command.id, command);
}

/** All registered commands regardless of role (diagnostic use only). */
export function getAllCommands(): CommandDescriptor[] {
  return Array.from(registry.values());
}

/**
 * Commands visible to a session with the given role.
 *
 * SEAM: CONST-27 (viewer role: `useAuthRole`/`RoleGate`) is not merged to main as of this item —
 * `useAuth()` on main has no `role` field at all. Rather than invent a role source here, the
 * caller (`CommandPalette.tsx`) passes `null` in that case, and `role: null` resolves to
 * **'operator'** — deliberately mirroring CONST-27's own server-side backward-compat rule ("a
 * claim-absent token resolves to operator", `src/constellation/auth.rs`), so this palette behaves
 * identically to every other pre-CONST-27 control in the app: no visible gating until the role
 * plumbing actually lands, at which point a real `'viewer'` role starts hiding operator-only
 * commands with no change needed here. When CONST-27 merges, replace the `null` passed in by the
 * caller with `useAuthRole()`'s real value — this function's contract does not need to change.
 * Operator-only commands are OMITTED entirely for a viewer session — not returned-but-disabled —
 * see the doc comment on `minRole` above.
 */
export function getAvailableCommands(role: 'operator' | 'viewer' | null): CommandDescriptor[] {
  const effectiveRole = role ?? 'operator';
  return getAllCommands().filter(c => (c.minRole ?? 'viewer') === 'viewer' || effectiveRole === 'operator');
}

/** Test/dev helper — not used by the shell at runtime. */
export function clearCommandRegistry(): void {
  registry.clear();
}
