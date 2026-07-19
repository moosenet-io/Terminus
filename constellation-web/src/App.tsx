// CONST-16: two-tier shell (GlobalBar + ModuleRail + card-canvas Overview), replacing the
// CONST-04 single-sidebar layout. Auth-gates on useAuth; health drives module availability
// (with a 2-cycle stale-while-degrading grace so one flaky poll never yanks a module's nav
// entry); routes ONLY the panels whose module is currently available — no hardcoded page table.
import { useEffect, useState, useCallback, useMemo, useRef } from 'react';
import { BrowserRouter, Routes, Route, Navigate, useLocation, useNavigate } from 'react-router-dom';
import { GlobalBar } from './components/GlobalBar';
import type { Density } from './components/GlobalBar';
import { ModuleRail } from './components/ModuleRail';
import type { RailVariant } from './components/ModuleRail';
import { Login } from './components/Login';
import { CommandPalette } from './components/CommandPalette';
import { useAuth } from './hooks/useAuth';
import { AuthRoleProvider, useAuthRole } from './hooks/AuthRoleContext';
import { getAggregationClient } from './lib/aggregationClient';
import type { HealthStatus } from './lib/aggregationClient';
import { getAvailableModules, getAvailablePanels } from './lib/moduleRegistry';
import { setCurrentPath, REFRESH_HEALTH_EVENT } from './lib/shellBridge';
import { OverviewPanel } from './panels/overview/OverviewPanel';

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
  const [drawerOpen, setDrawerOpen] = useState(false);
  // CONST-25: the command palette's open state lives here (not in GlobalBar) so Ctrl/Cmd+K
  // works everywhere the shell is mounted, not just while the bar has DOM focus.
  const [paletteOpen, setPaletteOpen] = useState(false);

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
  const panels = useMemo(
    () => getAvailablePanels().filter(p => availableModuleIds.has(p.system)),
    [availableModuleIds],
  );

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
  const activeModule = modules.find(m => m.id === activeModuleId) ?? null;

  const handleSelectModule = (id: string) => {
    const firstPanel = panels.find(p => p.system === id);
    navigate(firstPanel ? firstPanel.path : '/overview');
  };

  const handleDensityChange = (d: Density) => {
    setDensity(d);
    getAggregationClient().prefs.set('density', d);
  };

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100vh', overflow: 'hidden' }}>
      <GlobalBar
        modules={modules}
        health={health}
        degradedSystems={degradedSystems}
        activeModuleId={activeModuleId}
        onSelectModule={handleSelectModule}
        density={density}
        onDensityChange={handleDensityChange}
        username={username}
        onLogout={onLogout}
        pollDegraded={pollDegraded}
        onOpenMenu={railVariant === 'drawer' ? () => setDrawerOpen(true) : undefined}
        onOpenPalette={() => setPaletteOpen(true)}
      />

      <CommandPalette
        open={paletteOpen}
        onClose={() => setPaletteOpen(false)}
        panels={panels}
        onNavigate={navigate}
        // CONST-27 merged: the real session role now gates operator-only commands (a
        // viewer session hides/disables them; server-side 403 remains the enforcement).
        role={sessionRole}
      />

      <div style={{ flex: 1, display: 'flex', overflow: 'hidden', minHeight: 0 }}>
        {activeModule && (
          <ModuleRail
            module={activeModule}
            variant={railVariant}
            drawerOpen={drawerOpen}
            onCloseDrawer={() => setDrawerOpen(false)}
          />
        )}

        <div style={{ flex: 1, overflow: 'hidden', display: 'flex', flexDirection: 'column', minWidth: 0 }}>
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
                  <OverviewPanel modules={modules} health={health} degradedSystems={degradedSystems} density={density} />
                }
              />
              {panels.map(panel => {
                const Component = panel.component;
                return <Route key={panel.id} path={panel.path} element={<Component />} />;
              })}
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
  );
}

export default function App() {
  const { authenticated, username, role, loading: authLoading, login, logout } = useAuth();

  // While checking auth, show blank page (avoids flash of login screen)
  if (authLoading) {
    return <div style={{ height: '100vh', background: 'var(--bg-base)' }} />;
  }

  if (!authenticated) {
    return <Login onLogin={login} />;
  }

  return (
    <BrowserRouter basename="/">
      {/* CONST-27: republish `role` via context so `RoleGate` — used deep inside panels the
          router mounts with no props — can read it without prop-drilling. */}
      <AuthRoleProvider role={role}>
        <Shell username={username} onLogout={logout} />
      </AuthRoleProvider>
    </BrowserRouter>
  );
}
