// CONST-16: two-tier shell (GlobalBar + ModuleRail + card-canvas Overview), replacing the
// CONST-04 single-sidebar layout. Auth-gates on useAuth; health drives module availability
// (with a 2-cycle stale-while-degrading grace so one flaky poll never yanks a module's nav
// entry); routes ONLY the panels whose module is currently available — no hardcoded page table.
import { useEffect, useState, useCallback, useMemo, useRef } from 'react';
import { BrowserRouter, Routes, Route, Navigate, useLocation, useNavigate, useParams } from 'react-router-dom';
import { GlobalBar } from './components/GlobalBar';
import type { Density } from './components/GlobalBar';
import type { RailVariant } from './components/ModuleRail';
import { CoreRail } from './components/CoreRail';
import { DeepSpaceBackdrop } from './components/DeepSpaceBackdrop';
import { CORES, CORE_ORDER, coreForModule, getCore, modulesInCore } from './lib/cores';
import type { CoreId } from './lib/cores';
import { Login } from './components/Login';
import { CommandPalette } from './components/CommandPalette';
import { ToastProvider, useToastContext } from './components/Toast';
import { useAuth } from './hooks/useAuth';
import { useActivityFeed } from './hooks/useActivityFeed';
import { AuthRoleProvider, useAuthRole } from './hooks/AuthRoleContext';
import { getAggregationClient } from './lib/aggregationClient';
import type { HealthStatus } from './lib/aggregationClient';
import type { FeedItem } from './lib/activityFeed';
import { getAvailableModules, getAvailablePanels } from './lib/moduleRegistry';
import type { ModuleDescriptor, ModuleId } from './lib/moduleRegistry';
import { setCurrentPath, REFRESH_HEALTH_EVENT } from './lib/shellBridge';
import { contentMaxWidth } from './lib/catalogLayout';
import { OverviewPanel } from './panels/overview/OverviewPanel';
import { ModuleDetail } from './panels/overview/ModuleDetail';

/** CGUI-04 (TERM #527): the `/:moduleId/detail` route element — the reusable "Inside a client"
 *  detail view for whichever available module the operator drilled into (from an Overview card).
 *  An unknown/unavailable module id falls back to the overview, matching the wildcard's posture.
 *  The module id is the first path segment, so the shell's `activeModuleId` derivation keeps the
 *  module rail mounted — "same shell, deeper zoom". */
function ModuleDetailRoute({ modules, health }: { modules: ModuleDescriptor[]; health: HealthStatus[] }) {
  const { moduleId } = useParams();
  const module = modules.find(m => m.id === moduleId);
  if (!module) return <Navigate to="/overview" replace />;
  return <ModuleDetail module={module} health={health.find(h => h.system === module.healthSystem)} />;
}

/** A system stays reported `available` (degraded) through this many consecutive misses —
 *  whether an explicit `available:false`, disappearing from the health payload entirely, or a
 *  total poll failure — before the shell actually hides its module/nav entry on the NEXT
 *  (GRACE_CYCLES + 1-th) miss (§1.3 rule 2 / §10 CONST-16 "stale-while-degrading"). */
const GRACE_CYCLES = 2;

/** Responsive rail breakpoints (§3.1): icon rail below 1100px, drawer overlay below 760px. */
function railVariantFor(width: number): RailVariant {
  if (width >= 1100) return 'full';
  if (width >= 760) return 'icon';
  return 'drawer';
}

function useWindowWidth(): number {
  const [width, setWidth] = useState(() => window.innerWidth);
  useEffect(() => {
    const onResize = () => setWidth(window.innerWidth);
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
  }, []);
  return width;
}

function Shell({ username, onLogout }: { username: string | null; onLogout: () => void }) {
  // CONST-27's session role (from AuthRoleProvider above) — gates operator-only palette commands.
  const sessionRole = useAuthRole();
  const [health, setHealth] = useState<HealthStatus[]>([]);
  // Has the first /api/health poll settled (success OR failure) yet? Until it has, `modules`/
  // `panels` are necessarily empty (health starts as []) — routing on that empty snapshot would
  // treat every deep link as "module unavailable" and redirect it to /overview, losing it on
  // reload (review finding). So the Routes below don't mount at all until this is true; the
  // requested path sits untouched in the meantime.
  const [healthLoaded, setHealthLoaded] = useState(false);
  const [pollDegraded, setPollDegraded] = useState(false);
  const [degradedSystems, setDegradedSystems] = useState<Set<string>>(new Set());
  const [density, setDensity] = useState<Density>(
    () => getAggregationClient().prefs.get<Density>('density') ?? 'comfortable',
  );
  // S127 TGUI2 (§3.1): the active Overview core tab. Persisted client-side (the core model is the
  // real constellation grouping, see lib/cores.ts); restored on load, defaulting to the first core.
  const [activeCore, setActiveCore] = useState<CoreId>(() => {
    const saved = getAggregationClient().prefs.get<CoreId>('core');
    return saved && CORE_ORDER.includes(saved) ? saved : CORE_ORDER[0];
  });
  const [drawerOpen, setDrawerOpen] = useState(false);
  // CONST-25: the command palette's open state lives here (not in GlobalBar) so Ctrl/Cmd+K
  // works everywhere the shell is mounted, not just while the bar has DOM focus.
  const [paletteOpen, setPaletteOpen] = useState(false);

  // CONST-26 (§3.3): the shell's one merged activity feed, shared by the GlobalBar's
  // notification bell and the Overview canvas' ActivityFeedCard — a detected health transition
  // ALSO surfaces as a toast (via the ToastProvider mounted around this whole component in
  // `App()` below), which is why the toast-push callback lives here rather than inside the hook
  // itself (the hook stays toast-layer-agnostic).
  const { push: pushToast } = useToastContext();
  const feedItems = useActivityFeed(
    health,
    useCallback(
      (item: FeedItem) => pushToast(item.text.replace(/^\[(ok|warn|error)\]\s*/, ''), item.level),
      [pushToast],
    ),
  );

  // Grace bookkeeping: which systems have EVER been seen available (so a system that's never
  // come up doesn't get a fake grace window), and a per-system consecutive-miss counter.
  const everAvailable = useRef<Set<string>>(new Set());
  const missCounts = useRef<Map<string, number>>(new Map());

  /** Ages one system's grace window by one miss. Returns 'still-graced' while `misses <=
   *  GRACE_CYCLES` (caller should keep reporting it available), or 'expired' once the window
   *  has run out (caller should let it actually go unavailable). */
  const ageMiss = useCallback((system: string): 'still-graced' | 'expired' => {
    const misses = (missCounts.current.get(system) ?? 0) + 1;
    missCounts.current.set(system, misses);
    return misses <= GRACE_CYCLES ? 'still-graced' : 'expired';
  }, []);

  const applyGrace = useCallback((raw: HealthStatus[]): HealthStatus[] => {
    const degraded = new Set<string>();
    const seen = new Set<string>();
    const out = raw.map(h => {
      seen.add(h.system);
      if (h.available) {
        everAvailable.current.add(h.system);
        missCounts.current.set(h.system, 0);
        return h;
      }
      if (!everAvailable.current.has(h.system)) return h; // never confirmed up — no grace to extend
      if (ageMiss(h.system) === 'still-graced') {
        degraded.add(h.system);
        return { ...h, available: true };
      }
      return h;
    });

    // A previously-available system can also vanish from the payload ENTIRELY (not just flip
    // to available:false) — e.g. the backend drops its health-probe entry outright. Treat that
    // the same as an explicit miss: hold it through the grace window (synthesizing its entry)
    // before letting its module actually go unavailable (review finding: "absent from payload").
    for (const system of everAvailable.current) {
      if (seen.has(system)) continue;
      if (ageMiss(system) === 'still-graced') {
        degraded.add(system);
        out.push({
          system: system as HealthStatus['system'],
          available: true,
          detail: 'degraded (missing from health payload)',
        });
      }
      // else: past grace — leave it out of `out` entirely; its module naturally reports unavailable.
    }

    setDegradedSystems(degraded);
    return out;
  }, [ageMiss]);

  /** A TOTAL health-poll failure (the request itself threw) still has to age the grace clock —
   *  otherwise a system that was available before the backend went dark stays reported
   *  available forever, since no explicit available:false ever arrives to increment its miss
   *  count. Each failed poll counts as one miss cycle for every currently-tracked system, so
   *  after GRACE_CYCLES consecutive failures a stale module ages out exactly like an explicit
   *  per-system miss would (review finding: "poll failure never ages grace state"). */
  const ageOnPollFailure = useCallback(
    (prevHealth: HealthStatus[]): HealthStatus[] => {
      const degraded = new Set<string>();
      const seen = new Set<string>(prevHealth.map(h => h.system));
      const out: HealthStatus[] = [];

      for (const h of prevHealth) {
        if (!everAvailable.current.has(h.system)) {
          out.push(h); // never confirmed up — nothing to age, already unavailable
          continue;
        }
        if (ageMiss(h.system) === 'still-graced') {
          degraded.add(h.system);
          out.push({ ...h, available: true });
        }
        // else: past grace — drop it, its module goes unavailable.
      }

      // Defensive: age any tracked system that wasn't even in the last snapshot (shouldn't
      // normally happen, since applyGrace already folds vanished-but-graced systems in).
      for (const system of everAvailable.current) {
        if (seen.has(system)) continue;
        if (ageMiss(system) === 'still-graced') {
          degraded.add(system);
          out.push({
            system: system as HealthStatus['system'],
            available: true,
            detail: 'degraded (health poll unreachable)',
          });
        }
      }

      setDegradedSystems(degraded);
      return out;
    },
    [ageMiss],
  );

  const fetchHealth = useCallback(() => {
    getAggregationClient()
      .health.list()
      .then(raw => {
        setHealth(applyGrace(raw));
        setPollDegraded(false);
      })
      .catch(() => {
        // Health poll failed entirely: age the grace clock for every tracked system (see
        // ageOnPollFailure) and mark the bar degraded (§10 CONST-16 edge case) rather than
        // wiping the shell blank OR pinning everything available forever.
        setPollDegraded(true);
        setHealth(prev => ageOnPollFailure(prev));
      })
      .finally(() => setHealthLoaded(true));
  }, [applyGrace, ageOnPollFailure]);

  useEffect(() => {
    fetchHealth();
  }, [fetchHealth]);

  // CONST-25: the "Refresh health" palette command (registered at import time in
  // registerPanels.ts, well before this component exists) asks for a refresh via a plain
  // window CustomEvent — see lib/shellBridge.ts's doc comment for why.
  useEffect(() => {
    const handler = () => fetchHealth();
    window.addEventListener(REFRESH_HEALTH_EVENT, handler);
    return () => window.removeEventListener(REFRESH_HEALTH_EVENT, handler);
  }, [fetchHealth]);

  useEffect(() => {
    const id = setInterval(fetchHealth, 30000);
    return () => clearInterval(id);
  }, [fetchHealth]);

  // CONST-25: Ctrl/Cmd+K opens the palette from anywhere in the shell; Escape closes it here too
  // (in addition to the palette's own input-level Escape handler, so it also closes if focus is
  // somehow outside the input).
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault();
        setPaletteOpen(o => !o);
      } else if (
        // POL-12: bare "/" opens the palette (Stripe/GitHub pattern) — but only when the
        // operator is NOT typing into a field, so "/" stays literal in inputs/textareas/
        // contentEditable and the palette's own search box.
        e.key === '/' &&
        !e.metaKey &&
        !e.ctrlKey &&
        !e.altKey &&
        !(e.target instanceof HTMLInputElement) &&
        !(e.target instanceof HTMLTextAreaElement) &&
        !(e.target instanceof HTMLElement && e.target.isContentEditable)
      ) {
        e.preventDefault();
        setPaletteOpen(true);
      } else if (e.key === 'Escape') {
        setPaletteOpen(false);
      }
    };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, []);

  const width = useWindowWidth();
  const railVariant = railVariantFor(width);

  const modules = useMemo(() => getAvailableModules(health), [health]);
  const availableModuleIds = useMemo(() => new Set(modules.map(m => m.id as string)), [modules]);
  // Every routable panel. This list drives ROUTING and must stay complete — a
  // parameterized panel still needs its <Route>, it just must not be offered as a
  // navigation destination.
  const panels = useMemo(
    () => getAvailablePanels().filter(p => availableModuleIds.has(p.system)),
    [availableModuleIds],
  );

  // The subset offered as NAV DESTINATIONS (command palette). `hideInRail` names the
  // rail but means "not a nav destination": a palette entry for a parameterized route
  // navigates to the literal `/muse/library/:id`, which is not a page.
  //
  // This is deliberately a SECOND list rather than a filter on `panels` — filtering
  // `panels` itself removed the route entirely and the detail page 404'd to the module
  // overview. Caught by re-running the live verification after the fix, which is
  // exactly why the check is worth running twice.
  const navigablePanels = useMemo(() => panels.filter(p => !p.hideInRail), [panels]);

  const location = useLocation();
  const navigate = useNavigate();

  // CONST-25: publish the current path for the "Copy current path" command (see
  // lib/shellBridge.ts — deliberately routed through react-router's location, never
  // `window.location`, to keep that read confined to aggregationClient.ts).
  useEffect(() => {
    setCurrentPath(location.pathname);
  }, [location.pathname]);

  // Panel paths are all `/${moduleId}/...` by convention, so the first segment is the module id.
  const activeModuleId = useMemo(() => {
    const segment = location.pathname.split('/').filter(Boolean)[0];
    if (!segment || segment === 'overview') return null;
    return modules.find(m => m.id === segment)?.id ?? null;
  }, [location.pathname, modules]);

  const handleDensityChange = (d: Density) => {
    setDensity(d);
    getAggregationClient().prefs.set('density', d);
  };

  // S127: selecting a core scopes the Overview (rail + card canvas) to that core's member
  // modules and returns to the overview. Persisted so the shell reopens on the same core.
  const handleSelectCore = useCallback(
    (id: CoreId) => {
      setActiveCore(id);
      getAggregationClient().prefs.set('core', id);
      navigate('/overview');
    },
    [navigate],
  );

  // Keep the active core tab in sync when the operator drills into a panel from elsewhere
  // (deep link, card click, palette, rail) whose core differs from the current tab — the core
  // tab should always reflect where you are. Persist so a reload keeps the synced core.
  useEffect(() => {
    if (!activeModuleId) return;
    const core = coreForModule(activeModuleId as ModuleId);
    setActiveCore(prev => {
      if (prev === core) return prev;
      getAggregationClient().prefs.set('core', core);
      return core;
    });
  }, [activeModuleId]);

  // The core to render nav for: whichever a drilled-in panel belongs to, else the selected tab.
  // Using this derived value (not just `activeCore` state) makes the rail + tab highlight follow
  // a deep-link on the very first render, before the sync effect above has run.
  const effectiveCore = activeModuleId ? coreForModule(activeModuleId as ModuleId) : activeCore;
  // The active core's available member modules (in core order) — scopes the rail + card canvas.
  const coreModules = useMemo(() => modulesInCore(effectiveCore, modules), [effectiveCore, modules]);
  const coreDescriptor = getCore(effectiveCore);

  // MGUI-18: POL-03 centres the canvas and caps it at `--content-max` (~1280px). That reading
  // measure is right for a column of prose and charts and wrong for a catalog — on an
  // ultrawide it left the Muse poster wall as a 1280px strip with empty desk either side,
  // which is the operator's "the card area does not resize with the window". Panels opt in
  // via `PanelDescriptor.wide`; the decision itself is a pure, tested function so the
  // fallback (any unmatched path keeps the standard cap) cannot drift.
  const canvasMaxWidth = useMemo(
    () => contentMaxWidth(location.pathname, panels),
    [location.pathname, panels],
  );

  return (
    <div style={{ position: 'relative', zIndex: 1, display: 'flex', flexDirection: 'column', height: '100vh', overflow: 'hidden' }}>
      {/* CGUI-12 (§0): the fixed deep-space backdrop sits behind the whole shell (z-index:0);
          this shell column is z-index:1 above it, and its panels/cards carry their own opaque
          fills so the gradient/nebula/starfield reads through the translucent bar + canvas gaps. */}
      <DeepSpaceBackdrop />
      <GlobalBar
        cores={CORES}
        activeCoreId={effectiveCore}
        onSelectCore={handleSelectCore}
        density={density}
        onDensityChange={handleDensityChange}
        username={username}
        onLogout={onLogout}
        pollDegraded={pollDegraded}
        health={health}
        degradedSystems={degradedSystems}
        onOpenMenu={railVariant === 'drawer' ? () => setDrawerOpen(true) : undefined}
        onOpenPalette={() => setPaletteOpen(true)}
        feedItems={feedItems}
      />

      <CommandPalette
        open={paletteOpen}
        onClose={() => setPaletteOpen(false)}
        panels={navigablePanels}
        onNavigate={navigate}
        // CONST-27 merged: the real session role now gates operator-only commands (a
        // viewer session hides/disables them; server-side 403 remains the enforcement).
        role={sessionRole}
      />

      {/* position:relative + zIndex:1 makes this rail+canvas row a real stacking context ABOVE
          the deep-space backdrop (which is position:fixed;z-index:-1) — without it, this
          non-positioned in-flow row would be painted UNDER the positioned backdrop and obscured. */}
      <div style={{ position: 'relative', zIndex: 1, flex: 1, display: 'flex', overflow: 'hidden', minHeight: 0 }}>
        {/* Left rail (§3.1): always the active CORE's rail — a flat panel list for a single-member
            core, or labelled TERMINUS / MODELS / MINT sub-groups for Terminus. Rows navigate to
            their real panel path. Only mounted once health has loaded so it never flashes empty. */}
        {healthLoaded && (
          <CoreRail
            core={coreDescriptor}
            modules={coreModules}
            health={health}
            degradedSystems={degradedSystems}
            variant={railVariant}
            drawerOpen={drawerOpen}
            onCloseDrawer={() => setDrawerOpen(false)}
          />
        )}

        {/* CGUI-02 (TERM 525): the canvas is the scroll container — the global bar + module
            rail stay a fixed frame, only this column scrolls. Panels route through <PanelRoot>
            (height:100% + min-height:0 + overflow-y:auto) so they scroll internally; this
            overflow-y:auto is the safety net so any panel that does not manage its own scroll
            still scrolls here instead of clipping. overflow-x stays hidden (no sideways scroll). */}
        <div style={{ flex: 1, overflowY: 'auto', overflowX: 'hidden', display: 'flex', flexDirection: 'column', minWidth: 0 }}>
          {/* POL-03 (§3.7): cap the canvas content at --content-max (~1280px) and CENTER it.
              This is the single biggest "tech-demo → product" lever — short pages stop being
              anchored top-left in a huge canvas with a yawning void to the right; every panel
              now composes as a centered column. The wrapper is flex-column + min-height:0 so a
              panel's own PanelRoot scroll frame (height:100%) still fills and scrolls inside it. */}
          <div style={{ width: '100%', maxWidth: canvasMaxWidth, margin: '0 auto', flex: 1, minHeight: 0, minWidth: 0, display: 'flex', flexDirection: 'column' }}>
          {!healthLoaded ? (
            // First health poll hasn't settled yet — `modules`/`panels` are necessarily empty
            // right now (health starts as []). Render a loading placeholder WITHOUT mounting
            // any route (in particular no wildcard redirect), so a deep link / reload of a real
            // panel path (e.g. /harmony/dashboard) sits untouched until we actually know whether
            // its module is available (review finding: first-render route loss).
            <div
              style={{
                flex: 1,
                display: 'flex',
                alignItems: 'center',
                justifyContent: 'center',
                color: 'var(--text-tertiary)',
                fontSize: 'var(--text-base)',
              }}
            >
              Loading…
            </div>
          ) : (
            <Routes>
              <Route
                path="/overview"
                element={
                  <OverviewPanel
                    core={coreDescriptor}
                    modules={coreModules}
                    health={health}
                    degradedSystems={degradedSystems}
                    density={density}
                    feedItems={feedItems}
                  />
                }
              />
              {panels.map(panel => {
                const Component = panel.component;
                return <Route key={panel.id} path={panel.path} element={<Component />} />;
              })}
              {/* CGUI-04 (TERM #527): reusable module detail view. A static panel path like
                  /harmony/dashboard (registered above) out-ranks this param route for that exact
                  path; only /:moduleId/detail (e.g. /terminus/detail) resolves here. */}
              <Route path="/:moduleId/detail" element={<ModuleDetailRoute modules={modules} health={health} />} />

              {/* Backward-compat: the pre-CONST-16 'Status' panels lived at /status/*; keep old
                  bookmarks/links working by redirecting to their re-homed harmony.* paths. */}
              <Route path="/status/analytics" element={<Navigate to="/harmony/analytics" replace />} />
              <Route path="/status/engine-diagram" element={<Navigate to="/harmony/engine" replace />} />
              <Route path="/" element={<Navigate to="/overview" replace />} />
              {/* Open route of a hidden/unavailable module's panel → redirect to Overview
                  (§10 CONST-16 edge case) — its Route above simply isn't registered, so any
                  other path falls through to this wildcard. Only reachable once health has
                  loaded (see the !healthLoaded branch above), so this never fires against a
                  still-unknown module. */}
              <Route path="*" element={<Navigate to="/overview" replace />} />
            </Routes>
          )}
          </div>
        </div>
      </div>
    </div>
  );
}

export default function App() {
  const { authenticated, username, role, loading: authLoading, login, logout } = useAuth();

  // ToastProvider wraps every branch (not just the authenticated Shell) so its mounted state
  // never resets across an auth transition — harmless for the pre-auth branches below, which
  // simply never push a toast (only `Shell`, via `useActivityFeed`, does).
  return (
    <ToastProvider>
      {authLoading ? (
        // While checking auth, show blank page (avoids flash of login screen)
        <div style={{ height: '100vh', background: 'var(--bg-base)' }} />
      ) : !authenticated ? (
        <Login onLogin={login} />
      ) : (
        <BrowserRouter basename="/">
          {/* CONST-27: republish `role` via context so `RoleGate` — used deep inside panels the
              router mounts with no props — can read it without prop-drilling. */}
          <AuthRoleProvider role={role}>
            <Shell username={username} onLogout={logout} />
          </AuthRoleProvider>
        </BrowserRouter>
      )}
    </ToastProvider>
  );
}
