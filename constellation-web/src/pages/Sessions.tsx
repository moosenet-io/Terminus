// TRCI-03: Session grouping — list and detail views.
import { useState, useEffect } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';

interface Session {
  id: string;
  name: string;
  project: string;
  started_at: string;
  ended_at?: string;
  status: string;
  task_count: number;
  completed_count: number;
  failed_count: number;
  total_cost_usd: number;
  avg_quality: number;
  total_tokens: number;
  preset_notch: number;
}

export function Sessions() {
  const [sessions, setSessions] = useState<Session[]>([]);
  const [selected, setSelected] = useState<Session | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    getAggregationClient()
      .request<{ sessions?: Session[] }>('harmony', '/sessions')
      .then(d => { setSessions(d.sessions || []); setLoading(false); })
      .catch(() => setLoading(false));
  }, []);

  const statusColor = (s: string) => {
    if (s === 'active') return 'var(--h-green)';
    if (s === 'completed') return 'var(--h-teal)';
    if (s === 'interrupted') return 'var(--h-amber)';
    return 'var(--h-text-muted)';
  };

  return (
    <div style={{ padding: 16, overflowY: 'auto', height: '100%' }}>
      <h2 style={{ fontSize: 16, fontWeight: 600, color: 'var(--h-teal)', marginBottom: 16 }}>Sessions</h2>

      {loading ? <div className="h-skeleton" style={{ height: 100 }} /> : sessions.length === 0 ? (
        <div style={{ color: 'var(--h-text-muted)', fontSize: 13 }}>No sessions yet. Sessions are created automatically when you run a build.</div>
      ) : (
        <div style={{ display: 'grid', gap: 8 }}>
          {sessions.map(s => (
            <div key={s.id} onClick={() => setSelected(selected?.id === s.id ? null : s)}
              className="h-card" style={{ cursor: 'pointer', border: selected?.id === s.id ? '1px solid var(--h-teal)' : undefined }}>
              <div className="h-card-header" style={{ cursor: 'pointer' }}>
                <div>
                  <span style={{ fontWeight: 600, fontSize: 13 }}>{s.name}</span>
                  <span style={{ fontSize: 11, color: 'var(--h-text-muted)', marginLeft: 8 }}>{s.project}</span>
                </div>
                <div style={{ display: 'flex', gap: 12, alignItems: 'center', fontSize: 12 }}>
                  <span style={{ color: statusColor(s.status) }}>{s.status}</span>
                  <span style={{ color: 'var(--h-text-muted)' }}>{s.started_at.slice(0, 10)}</span>
                </div>
              </div>
              {selected?.id === s.id && (
                <div className="h-card-body">
                  <div style={{ display: 'grid', gridTemplateColumns: 'repeat(4, 1fr)', gap: 8 }}>
                    {[
                      ['Tasks', s.task_count],
                      ['Completed', s.completed_count],
                      ['Failed', s.failed_count],
                      ['Cost', `$${s.total_cost_usd.toFixed(4)}`],
                      ['Avg Quality', s.avg_quality.toFixed(2)],
                      ['Tokens', s.total_tokens.toLocaleString()],
                      ['Preset', `notch ${s.preset_notch}/10`],
                      ['Duration', s.ended_at
                        ? `${Math.round((new Date(s.ended_at).getTime() - new Date(s.started_at).getTime()) / 60000)}m`
                        : 'ongoing'],
                    ].map(([label, value]) => (
                      <div key={String(label)} style={{ textAlign: 'center' }}>
                        <div style={{ fontSize: 15, fontWeight: 600, color: 'var(--h-teal)' }}>{value}</div>
                        <div style={{ fontSize: 10, color: 'var(--h-text-muted)', marginTop: 2 }}>{label}</div>
                      </div>
                    ))}
                  </div>
                </div>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
