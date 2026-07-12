// CONST-04: Thin registry-panel wrapper around the ported harmony-web Dashboard page, which
// expects status/executorSummary/loading/error/onRetry as props (originally threaded down from
// harmony-web's App.tsx). Supplies them from the shared useHarmonyStatus hook instead.
import { Dashboard } from '../../pages/Dashboard';
import { useHarmonyStatus } from '../../hooks/useHarmonyStatus';

export function DashboardPanel() {
  const { status, loading, error, executorSummary, refetch } = useHarmonyStatus();
  return (
    <Dashboard
      status={status}
      executorSummary={executorSummary}
      loading={loading}
      error={error}
      onRetry={refetch}
    />
  );
}
