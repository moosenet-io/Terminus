// CONST-04: Application shell — adapted from harmony-web's App.tsx.
// Auth-gates on useAuth, renders Sidebar + StatusStrip, and routes ONLY the panels the module
// registry reports as available (getAvailablePanels/getPanelsBySystem) — no hardcoded page
// table. An unregistered or unavailable capability simply isn't in the route list.
import { useEffect, useState, useCallback } from 'react';
import { BrowserRouter, Routes, Route, Navigate } from 'react-router-dom';
import { Sidebar } from './components/Sidebar';
import { StatusStrip } from './components/StatusStrip';
import { Login } from './components/Login';
import { useAuth } from './hooks/useAuth';
import { getAggregationClient } from './lib/aggregationClient';
import type { HealthStatus } from './lib/aggregationClient';
import { getAvailablePanels } from './lib/moduleRegistry';

export default function App() {
  const { authenticated, username, loading: authLoading, login, logout } = useAuth();
  const [health, setHealth] = useState<HealthStatus[]>([]);
  const [healthLoading, setHealthLoading] = useState(true);

  const fetchHealth = useCallback(() => {
    setHealthLoading(true);
    getAggregationClient()
      .health.list()
      .then(setHealth)
      .catch(() => setHealth([]))
      .finally(() => setHealthLoading(false));
  }, []);

  useEffect(() => {
    if (authenticated && !authLoading) fetchHealth();
  }, [authenticated, authLoading, fetchHealth]);

  // Periodic safety-net poll for system health.
  useEffect(() => {
    if (!authenticated || authLoading) return;
    const id = setInterval(fetchHealth, 30000);
    return () => clearInterval(id);
  }, [authenticated, authLoading, fetchHealth]);

  // While checking auth, show blank page (avoids flash of login screen)
  if (authLoading) {
    return <div style={{ height: '100vh', background: 'var(--bg-base)' }} />;
  }

  if (!authenticated) {
    return <Login onLogin={login} />;
  }

  const panels = getAvailablePanels();
  const firstPath = panels[0]?.path ?? '/';

  return (
    <BrowserRouter basename="/">
      <div style={{ display: 'flex', height: '100vh', overflow: 'hidden' }}>
        <Sidebar username={username} onLogout={logout} />

        <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden', minWidth: 0 }}>
          <StatusStrip health={health} loading={healthLoading} />

          <div style={{ flex: 1, overflow: 'hidden', display: 'flex', flexDirection: 'column' }}>
            {panels.length === 0 ? (
              <div style={{
                flex: 1, display: 'flex', alignItems: 'center', justifyContent: 'center',
                color: 'var(--text-tertiary)', fontSize: 'var(--text-base)',
              }}>
                No panels are currently available.
              </div>
            ) : (
              <Routes>
                {panels.map(panel => {
                  const Component = panel.component;
                  return <Route key={panel.id} path={panel.path} element={<Component />} />;
                })}
                {!panels.some(p => p.path === '/') && (
                  <Route path="/" element={<Navigate to={firstPath} replace />} />
                )}
                <Route path="*" element={<Navigate to={firstPath} replace />} />
              </Routes>
            )}
          </div>
        </div>
      </div>
    </BrowserRouter>
  );
}
