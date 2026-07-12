// ACARD-02: Container for 4 agent lanes — polls /api/agents/activity every 5s
import { useEffect, useState } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import type { AgentActivity, AgentActivityResponse } from '../types/api';
import { AgentLane } from './AgentLane';

function LaneSkeleton() {
  return (
    <div className="h-skeleton" style={{ minWidth: 180, flex: '1 1 0', height: 200, borderRadius: 8 }} />
  );
}

export function AgentActivityCard() {
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
          // Sort active agents first
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
          setError(e instanceof Error ? e.message : 'Failed to load agent activity');
          setLoading(false);
        });
    };

    load();
    const id = setInterval(load, 5000);
    return () => { cancelled = true; clearInterval(id); };
  }, []);

  const activeCount = agents.filter(a => a.status === 'active').length;

  return (
    <div className="h-card">
      <div className="h-card-header" style={{ cursor: 'default' }}>
        <div className="h-flex h-gap-sm">
          <span className={`h-dot ${activeCount > 0 ? 'h-dot-green h-pulse' : 'h-dot-dim'}`} />
          <span style={{ fontWeight: 600, fontSize: 13 }}>Agent Activity</span>
          {activeCount > 0 && (
            <span style={{
              fontSize: 11,
              padding: '1px 6px',
              borderRadius: 10,
              background: 'var(--accent-primary-subtle)',
              color: 'var(--accent-primary)',
              fontWeight: 600,
            }}>
              {activeCount} active
            </span>
          )}
        </div>
        {error && (
          <span style={{ fontSize: 11, color: 'var(--status-warning)' }}>⚠ {error}</span>
        )}
      </div>

      <div className="h-card-body" style={{ padding: '0 12px 12px' }}>
        <div style={{
          display: 'flex',
          gap: 12,
          overflowX: 'auto',
          paddingBottom: 4,
        }}>
          {loading ? (
            <>
              <LaneSkeleton />
              <LaneSkeleton />
              <LaneSkeleton />
              <LaneSkeleton />
            </>
          ) : agents.length === 0 ? (
            <div style={{ color: 'var(--text-tertiary)', fontSize: 13, padding: '16px 4px' }}>
              No agent data available
            </div>
          ) : (
            agents.map(agent => (
              <AgentLane key={agent.agent_id} agent={agent} />
            ))
          )}
        </div>
      </div>
    </div>
  );
}
