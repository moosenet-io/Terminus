// ACARD-06: Agents page — full-width agent lanes using AgentActivityCard data
import { useEffect, useState } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import type { AgentActivity, AgentActivityResponse } from '../types/api';
import { AgentLane } from '../components/AgentLane';

export function Agents() {
  const [agents, setAgents] = useState<AgentActivity[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;

    const load = () => {
      getAggregationClient()
        .request<AgentActivityResponse>('harmony', '/agents/activity')
        .then((d: AgentActivityResponse) => {
          if (cancelled) return;
          // Active agents first
          const sorted = [...d.agents].sort((a, b) => {
            if (a.status === 'active' && b.status !== 'active') return -1;
            if (b.status === 'active' && a.status !== 'active') return 1;
            return 0;
          });
          setAgents(sorted);
          setLoading(false);
          setError(null);
        })
        .catch((e: unknown) => {
          if (cancelled) return;
          setError(e instanceof Error ? e.message : 'Failed to load agents');
          setLoading(false);
        });
    };

    load();
    const id = setInterval(load, 5000);
    return () => { cancelled = true; clearInterval(id); };
  }, []);

  const activeCount = agents.filter(a => a.status === 'active').length;

  return (
    <div style={{ padding: 16, overflowY: 'auto', height: '100%', display: 'flex', flexDirection: 'column', gap: 12 }}>
      {/* Page header */}
      <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
        <h2 style={{ fontSize: 16, fontWeight: 600, color: 'var(--accent-primary)', margin: 0 }}>
          Agents
        </h2>
        {activeCount > 0 && (
          <span style={{
            fontSize: 11,
            padding: '2px 8px',
            borderRadius: 10,
            background: 'var(--accent-primary-subtle)',
            color: 'var(--accent-primary)',
            fontWeight: 600,
          }}>
            {activeCount} active
          </span>
        )}
        {error && (
          <span style={{ fontSize: 11, color: 'var(--status-warning)', marginLeft: 'auto' }}>
            ⚠ {error}
          </span>
        )}
      </div>

      {/* Lane grid */}
      {loading ? (
        <div style={{ display: 'flex', gap: 12 }}>
          {[1, 2, 3, 4].map(i => (
            <div key={i} className="h-skeleton" style={{ flex: '1 1 0', height: 220, borderRadius: 8 }} />
          ))}
        </div>
      ) : agents.length === 0 ? (
        <div style={{ color: 'var(--text-tertiary)', fontSize: 13, padding: 16 }}>
          No agent data available.
        </div>
      ) : (
        <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap' }}>
          {agents.map(agent => (
            <div key={agent.agent_id} style={{ flex: '1 1 220px' }}>
              <AgentLane agent={agent} />
              {/* Stats section for active agents */}
              {agent.status === 'active' && (
                <div style={{
                  marginTop: 6,
                  padding: '8px 12px',
                  background: 'var(--bg-surface)',
                  border: '1px solid var(--border-subtle)',
                  borderRadius: 6,
                  fontSize: 11,
                  color: 'var(--text-secondary)',
                  display: 'flex',
                  gap: 16,
                  flexWrap: 'wrap',
                }}>
                  {agent.elapsed_seconds > 0 && (
                    <span>
                      Elapsed:{' '}
                      <span style={{ color: 'var(--text-primary)', fontFamily: 'var(--font-mono)' }}>
                        {Math.floor(agent.elapsed_seconds / 60)}m{String(agent.elapsed_seconds % 60).padStart(2, '0')}s
                      </span>
                    </span>
                  )}
                  {agent.task && (
                    <span>
                      Task:{' '}
                      <span style={{ color: 'var(--accent-primary)', fontFamily: 'var(--font-mono)' }}>
                        {agent.task.id}
                      </span>
                      {' — '}
                      <span style={{ color: 'var(--text-primary)' }}>{agent.task.title}</span>
                    </span>
                  )}
                  {agent.loop_state && (
                    <span>
                      Phase:{' '}
                      <span style={{ color: 'var(--status-info)' }}>{agent.loop_state.phase}</span>
                      {' '}
                      <span style={{ color: 'var(--text-tertiary)' }}>
                        ({agent.loop_state.iteration}/{agent.loop_state.max_iterations})
                      </span>
                    </span>
                  )}
                </div>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
