// SGUI-06: Recent activity feed
interface ActivityEntry { id: string; type: 'done' | 'pr' | 'running' | 'waiting' | 'queued'; text: string; time: string; }

interface Props { entries: ActivityEntry[]; }

const TYPE_ICON = { done: '✓', pr: '↗', running: '●', waiting: '⌛', queued: '○' };
const TYPE_COLOR = { done: 'var(--h-green)', pr: 'var(--h-blue)', running: 'var(--h-teal)', waiting: 'var(--h-amber)', queued: 'var(--h-text-muted)' };

export function ActivityFeed({ entries }: Props) {
  return (
    <div className="h-card" style={{ overflow: 'hidden', display: 'flex', flexDirection: 'column' }}>
      <div className="h-card-header" style={{ cursor: 'default' }}>
        <span style={{ fontWeight: 600, fontSize: 13 }}>Activity</span>
      </div>
      <div style={{ flex: 1, overflowY: 'auto', maxHeight: 280 }}>
        {entries.length === 0 ? (
          <div style={{ color: 'var(--h-text-muted)', textAlign: 'center', padding: 16, fontSize: 13 }}>No recent activity</div>
        ) : (
          entries.map(e => (
            <div key={e.id} style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '6px 14px', borderBottom: '1px solid var(--h-border)' }}>
              <span style={{ color: TYPE_COLOR[e.type], fontSize: 12, flexShrink: 0, width: 14 }}>{TYPE_ICON[e.type]}</span>
              <span className="h-truncate h-flex-1" style={{ fontSize: 12, color: 'var(--h-text)' }}>{e.text}</span>
              <span className="h-mono" style={{ fontSize: 11, color: 'var(--h-text-muted)', flexShrink: 0 }}>{e.time}</span>
            </div>
          ))
        )}
      </div>
    </div>
  );
}
