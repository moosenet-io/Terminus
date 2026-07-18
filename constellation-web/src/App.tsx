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
import { useAuth } from './hooks/useAuth';
import { getAggregationClient } from './lib/aggregationClient';
import type { HealthStatus } from './lib/aggregationClient';
import { getAvailableModules, getAvailablePanels } from './lib/moduleRegistry';
import { OverviewPanel } from './panels/overview/OverviewPanel';

/** A system stays reported `available` for this many consecutive failed polls before the shell
 *  actually hides its module/nav entry (§1.3 rule 2 / §10 CONST-16 "stale-while-degrading"). */
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
  const [health, setHealth] = useState<HealthStatus[]>([]);
  const [pollDegraded, setPollDegraded] = useState(false);
  const [degradedSystems, setDegradedSystems] = useState<Set<string>>(new Set());
  const [density, setDensity] = useState<Density>(
    () => getAggregationClient().prefs.get<Density>('density') ?? 'comfortable',
  );
  const [drawerOpen, setDrawerOpen] = useState(false);

  // Grace bookkeeping: which systems have EVER been seen available (so a system that's never
  // come up doesn't get a fake grace window), and a per-system consecutive-miss counter.
  const everAvailable = useRef<Set<string>>(new Set());
  const missCounts = useRef<Map<string, number>>(new Map());

  const applyGrace = useCallback((raw: HealthStatus[]): HealthStatus[] => {
    const degraded = new Set<string>();
    const out = raw.map(h => {
      if (h.available) {
        everAvailable.current.add(h.system);
        missCounts.current.set(h.system, 0);
        return h;
      }
      if (!everAvailable.current.has(h.system)) return h; // never confirmed up — no grace to extend
      const misses = (missCounts.current.get(h.system) ?? 0) + 1;
      missCounts.current.set(h.system, misses);
      if (misses <= GRACE_CYCLES) {
        degraded.add(h.system);
        return { ...h, available: true };
      }
      return h;
    });
    setDegradedSystems(degraded);
    return out;
  }, []);

  const fetchHealth = useCallback(() => {
    getAggregationClient()
      .health.list()
      .then(raw => {
        setHealth(applyGrace(raw));
        setPollDegraded(false);
      })
      .catch(() => {
        // Health poll failed entirely: keep the last known health/availability, just mark the
        // bar degraded (§10 CONST-16 edge case) rather than wiping the shell blank.
        setPollDegraded(true);
      });
  }, [applyGrace]);

  useEffect(() => {
    fetchHealth();
  }, [fetchHealth]);

  useEffect(() => {
    const id = setInterval(fetchHealth, 30000);
    return () => clearInterval(id);
  }, [fetchHealth]);

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
            <Route path="/" element={<Navigate to="/overview" replace />} />
            {/* Open route of a hidden/unavailable module's panel → redirect to Overview
                (§10 CONST-16 edge case) — its Route above simply isn't registered, so any
                other path falls through to this wildcard. */}
            <Route path="*" element={<Navigate to="/overview" replace />} />
          </Routes>
        </div>
      </div>
    </div>
  );
}

export default function App() {
  const { authenticated, username, loading: authLoading, login, logout } = useAuth();

  // While checking auth, show blank page (avoids flash of login screen)
  if (authLoading) {
    return <div style={{ height: '100vh', background: 'var(--bg-base)' }} />;
  }

  if (!authenticated) {
    return <Login onLogin={login} />;
  }

  return (
    <BrowserRouter basename="/">
      <Shell username={username} onLogout={logout} />
    </BrowserRouter>
  );
}
