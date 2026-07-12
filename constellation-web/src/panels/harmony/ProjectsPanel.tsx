// CONST-04: Thin registry-panel wrapper around the ported harmony-web Projects page, which
// expects engineState/isEnriching/liveProjects as props (originally threaded down from
// harmony-web's App.tsx). Supplies them from the shared useHarmonyStatus hook instead.
import { Projects } from '../../pages/Projects';
import { useHarmonyStatus } from '../../hooks/useHarmonyStatus';

export function ProjectsPanel() {
  const { status, isEnriching } = useHarmonyStatus();
  return (
    <Projects
      engineState={status?.engine_state ?? 'STOPPED'}
      isEnriching={isEnriching}
      liveProjects={status?.projects}
    />
  );
}
