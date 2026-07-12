// CONST-04: Registry-panel wrapper around the ported harmony-web dashboard/EnginePanel
// (worker -> engine routing diagram), grouped under Status per the CONST-04 nav mapping
// ("Status: Analytics (+ the dashboard/* engine view)"). Sources live executor state from the
// same shared hook the Harmony Dashboard/Projects panels use.
import { EnginePanel } from '../../components/dashboard/EnginePanel';
import { useHarmonyStatus } from '../../hooks/useHarmonyStatus';

export function EngineDiagramPanel() {
  const { executorSummary } = useHarmonyStatus();
  return (
    <div style={{ padding: 'var(--space-5)' }}>
      <EnginePanel summary={executorSummary} />
    </div>
  );
}
