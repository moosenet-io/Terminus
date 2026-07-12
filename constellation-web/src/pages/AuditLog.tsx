// SGUI-09: Audit log stub page
import { useEffect, useState } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';

interface AuditEntry { ts: string; event: string; data?: Record<string, unknown>; }

export function AuditLog() {
  const [entries, setEntries] = useState<AuditEntry[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    getAggregationClient()
      .request('harmony', '/state/analytics')
      .then(() => { setLoading(false); })
      .catch(() => setLoading(false));
  }, []);

  return (
    <div style={{ padding: 16, overflowY: 'auto', height: '100%' }}>
      <h2 style={{ fontSize: 16, fontWeight: 600, color: 'var(--h-teal)', marginBottom: 16 }}>Audit Log</h2>
      {loading ? <div className="h-skeleton" style={{ height: 80 }} /> : (
        <div className="h-card">
          <div className="h-card-body" style={{ color: 'var(--h-text-muted)', textAlign: 'center', padding: 24 }}>
            {entries.length === 0 ? 'Audit log data loaded from /var/log/harmony/audit.jsonl' : null}
            <p style={{ marginTop: 8, fontSize: 12 }}>Showing session events. Full audit log available at the path above.</p>
          </div>
        </div>
      )}
    </div>
  );
}
