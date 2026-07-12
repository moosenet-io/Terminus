// SGUI-06: Dashboard page assembly
// RENAME-04: Uses ModeSelector instead of InferenceMixSlider.
// ACARD-02: Row 4 uses AgentActivityCard instead of RoutingDiagram.
// GROW-05: Row 5 shows Task Tree visualization.
// TRIAGE-06: Row 0 shows triage mode indicator + held tasks + escalation stepper.
// WIRE-07: Engine diagram panel added alongside ExecutorCard.
// VLLM-07: EngineControls added to Row 1 for engine lifecycle management.
import type { StatusResponse } from '../types/api';
import { ExecutorCard } from '../components/ExecutorCard';
import { EngineControls } from '../components/EngineControls';
import { ProviderSummary } from '../components/ProviderSummary';
import { ModeSelector } from '../components/ModeSelector';
import { ProjectsCard } from '../components/ProjectsCard';
import { ActivityFeed } from '../components/ActivityFeed';
import { AgentActivityCard } from '../components/AgentActivityCard';
import { TaskTree, type TaskItem } from '../components/TaskTree';
import { HeldTasksPanel } from '../components/HeldTasksPanel';
import { EscalationStepper } from '../components/EscalationStepper';
import { EnginePanel } from '../components/dashboard/EnginePanel';
import { useTreeData } from '../hooks/useTreeData';
import type { ExecutorSummary } from '../hooks/useExecutorState';
import type { HeldTask } from '../types/engine';

interface Props {
  status: StatusResponse | null;
  executorSummary: ExecutorSummary;
  loading: boolean;
  error: string | null;
  onRetry: () => void;
}

/** Map a tree API phase string to the status expected by TaskTree/TreeNode. */
function phaseToStatus(phase: string): TaskItem['status'] {
  if (phase === 'done') return 'complete';
  if (phase === 'backlog') return 'pending';
  return 'active';
}

export function Dashboard({ status, executorSummary, loading, error, onRetry }: Props) {
  // Derive the most active project identifier for the tree (first project with in-progress work).
  const activeProject = status?.projects?.find(p => (p.counts?.in_progress ?? 0) > 0)?.identifier
    ?? status?.projects?.[0]?.identifier
    ?? 'LM';

  const { data: treeData } = useTreeData(activeProject);

  // TRIAGE-06: extract triage state from executor state
  const triageState = (status?.executor as unknown as { triage?: { active?: boolean; triage?: { current_task_id?: string; current_step?: number; current_step_name?: string; held_count?: number; step_outcomes?: unknown[] } } })?.triage;
  const isInTriageMode = triageState?.active === true;
  const triageDetail = triageState?.triage;
  const heldTasks: HeldTask[] = (status?.executor as unknown as { hold_state?: { held_tasks?: HeldTask[]; blocking_tasks?: string[]; dependency_gate_blocked?: boolean } })?.hold_state?.held_tasks ?? [];
  const blockingTasks: string[] = (status?.executor as unknown as { hold_state?: { blocking_tasks?: string[] } })?.hold_state?.blocking_tasks ?? [];

  // Flatten tree API response into TaskItem[] for TaskTree component.
  // Each spec becomes a root node (trunk); each item becomes a child of its spec.
  // LIVE-04: Thread stages, held, elapsed_secs, itemId, specTitle through.
  const treeItems: TaskItem[] = [];
  for (const spec of treeData?.specs ?? []) {
    if (spec.items.length === 0) continue;

    // Derive a spec-level status: active if any item is active, else complete if all done.
    const hasActive = spec.items.some((i) => phaseToStatus(i.phase) === 'active');
    const allDone = spec.items.every((i) => phaseToStatus(i.phase) === 'complete');
    const specStatus: TaskItem['status'] = hasActive ? 'active' : allDone ? 'complete' : 'pending';

    treeItems.push({
      id: spec.prefix,
      label: spec.prefix,
      status: specStatus,
      isSpec: true,
      specTitle: spec.spec_title ?? '',
    });

    for (const item of spec.items) {
      // Item ID badge: project prefix + numeric id, e.g. "LM-42"
      const projPrefix = spec.prefix.split('-')[0];
      const itemId = `${projPrefix}-${item.id}`;
      // Short title: strip the "PREFIX-NN: " prefix if present
      const shortTitle = item.title.replace(/^[A-Z0-9]+-\d+:\s*/, '').slice(0, 30);
      treeItems.push({
        id: String(item.id),
        label: shortTitle,
        status: phaseToStatus(item.phase),
        parentId: spec.prefix,
        itemId,
        stages: item.stages,
        held: item.held,
        elapsed_secs: item.elapsed_secs,
      });
    }
  }

  if (loading) {
    return (
      <div style={{ padding: 24, display: 'flex', flexDirection: 'column', gap: 16 }}>
        {[1,2,3,4].map(i => (
          <div key={i} className="h-skeleton" style={{ height: 80, borderRadius: 8 }} />
        ))}
      </div>
    );
  }

  if (error) {
    return (
      <div style={{ padding: 32, textAlign: 'center' }}>
        <div style={{ color: 'var(--h-red)', marginBottom: 12 }}>⚠ {error}</div>
        <button className="h-btn h-btn-ghost" onClick={onRetry}>Retry</button>
      </div>
    );
  }

  // Mock activity from status
  const activities = status?.projects?.slice(0, 5).map((p, i) => ({
    id: String(i),
    type: (p.counts?.in_progress || 0) > 0 ? 'running' as const : (p.counts?.done || 0) > 0 ? 'done' as const : 'queued' as const,
    text: `${p.identifier}: ${p.counts?.done || 0}/${(p.counts?.todo||0)+(p.counts?.in_progress||0)+(p.counts?.done||0)} done`,
    time: 'now',
  })) || [];

  return (
    <div style={{ padding: 16, display: 'flex', flexDirection: 'column', gap: 12, overflowY: 'auto', height: '100%' }}>
      {/* TRIAGE-06: Row 0 — triage mode indicator (only when in triage) */}
      {isInTriageMode && triageDetail && (
        <div className="h-card triage-pulse" style={{
          border: '2px solid var(--status-warning)',
          padding: '8px 12px',
        }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 6 }}>
            <span style={{
              fontWeight: 700, fontSize: 13,
              color: 'var(--status-warning)',
              letterSpacing: '0.05em',
            }}>⚠ TRIAGE MODE</span>
            <span style={{ color: 'var(--text-secondary)', fontSize: 12 }}>
              Resolving {triageDetail.current_task_id} — Step {triageDetail.current_step}/5 ({triageDetail.current_step_name}) — {triageDetail.held_count} task{triageDetail.held_count !== 1 ? 's' : ''} held
            </span>
          </div>
          <EscalationStepper
            currentStep={triageDetail.current_step ?? 0}
            stepOutcomes={(triageDetail.step_outcomes ?? []) as import('../types/engine').TriageStepOutcome[]}
          />
        </div>
      )}

      {/* VLLM-07: Engine lifecycle control bar */}
      <EngineControls
        engineState={status?.engine_state}
        activeWorkers={status?.workers ?? 0}
      />

      {/* Row 1: Executor + Provider Summary */}
      <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 }}>
        <ExecutorCard summary={executorSummary} />
        <ProviderSummary />
      </div>

      {/* WIRE-07: Engine diagram panel */}
      <EnginePanel summary={executorSummary} />

      {/* Row 2: Operating mode (RENAME-04) */}
      <ModeSelector initialMode={status?.mode ?? 'local'} />

      {/* Row 3: Projects + Activity */}
      <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 }}>
        <ProjectsCard
          projects={status?.projects || []}
          cached={status?.cached}
          cachedAgoSecs={status?.cached_ago_secs}
          loading={status?.loading}
        />
        <ActivityFeed entries={activities} />
      </div>

      {/* Row 4: Agent activity */}
      <AgentActivityCard />

      {/* TRIAGE-06: Held tasks panel (visible when there are held tasks) */}
      {heldTasks.length > 0 && (
        <HeldTasksPanel
          heldTasks={heldTasks}
          blockingTasks={blockingTasks}
          defaultExpanded={isInTriageMode}
        />
      )}

      {/* Row 5: Task tree — organic visualization of spec execution pipeline.
          Always renders; shows a seed placeholder when no active specs. */}
      <div className="h-card" style={{ padding: '12px 16px' }}>
        <div className="h-card-header" style={{ marginBottom: 8 }}>
          <span style={{ fontWeight: 600, fontSize: 'var(--text-md)', color: 'var(--text-primary)' }}>
            Task Tree
          </span>
          {treeItems.length > 0 && (
            <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)', marginLeft: 8 }}>
              {activeProject}
            </span>
          )}
        </div>
        {treeItems.length > 0 ? (
          <TaskTree items={treeItems} width={760} />
        ) : (
          <div style={{
            textAlign: 'center',
            padding: '24px 16px',
            color: 'var(--text-tertiary)',
            fontSize: 'var(--text-sm)',
          }}>
            <div style={{ fontSize: 32, marginBottom: 8 }}>🌱</div>
            <div style={{ fontWeight: 500 }}>No active specs</div>
            <div style={{ fontSize: 'var(--text-xs)', marginTop: 4 }}>
              Start a build to grow the tree
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
