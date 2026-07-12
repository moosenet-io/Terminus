// WIRE-07: Engine diagram — visual worker → engine routing panel
import { WorkerNode } from './WorkerNode';
import { EngineNode } from './EngineNode';
import type { ExecutorSummary } from '../../hooks/useExecutorState';

interface Props { summary: ExecutorSummary; }

const ENGINES = [
  { name: 'llama-server', providers: ['llama'] },
  { name: 'Ollama GPU', providers: ['local', 'ollama', 'gemini', 'claude', 'codex'] },
  { name: 'Ollama CPU', providers: ['cpu'] },
];

export function EnginePanel({ summary }: Props) {
  const workers = summary?.workers ?? [];
  const activeCount = summary?.activeCount ?? 0;

  return (
    <div className="h-card" style={{ padding: 12 }}>
      <div style={{ fontWeight: 600, marginBottom: 8 }}>Engine Diagram</div>
      <div style={{ display: 'flex', gap: 16, alignItems: 'flex-start', overflowX: 'auto', paddingBottom: 4 }}>
        {/* Worker nodes */}
        <div style={{ display: 'flex', flexDirection: 'column', gap: 8, flexShrink: 0 }}>
          <div style={{ fontSize: 10, color: 'var(--text-tertiary)', marginBottom: 2 }}>Workers</div>
          {workers.map(w => (
            <WorkerNode key={w.id} worker={w} />
          ))}
          {workers.length === 0 && (
            <div style={{ color: 'var(--text-tertiary)', fontSize: 11, padding: 8 }}>Engine idle</div>
          )}
        </div>

        {/* Spacer for connections */}
        <div style={{ width: 40 }} />

        {/* Engine nodes */}
        <div style={{ display: 'flex', flexDirection: 'column', gap: 8, flexShrink: 0 }}>
          <div style={{ fontSize: 10, color: 'var(--text-tertiary)', marginBottom: 2 }}>Engines</div>
          {ENGINES.map(e => (
            <EngineNode
              key={e.name}
              name={e.name}
              status={activeCount > 0 ? 'online' : 'online'}
            />
          ))}
        </div>
      </div>
    </div>
  );
}
