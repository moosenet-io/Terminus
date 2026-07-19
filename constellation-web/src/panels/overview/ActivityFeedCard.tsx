// CONST-26 (§3.3): the Overview canvas' activity-feed widget. Purely a renderer over the
// already-merged `FeedItem[]` the shell computes via `hooks/useActivityFeed.ts` -- this
// component owns no polling/subscriptions itself, so it never opens a second copy of the same
// timer/socket the status-strip bell (`components/NotificationBell.tsx`) also reads from.
//
// Deliberately NOT a drag/reorder/hide `ModuleCard` (CONST-16's layout-prefs system is scoped to
// registered `ModuleId`s only, and the prefs seam itself is layout/density-only per that item's
// contract) -- this renders as a fixed, always-visible extra cell in the Overview grid.
import type { FeedItem } from '../../lib/activityFeed';

const LEVEL_COLOR: Record<FeedItem['level'], string> = {
  ok: 'var(--status-success)',
  warn: 'var(--status-warning)',
  error: 'var(--status-error)',
};

function formatTime(ts: string): string {
  const d = new Date(ts);
  if (Number.isNaN(d.getTime())) return ts;
  return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
}

interface ActivityFeedCardProps {
  items: FeedItem[];
  /** How many rows to render -- the widget itself never fetches more than the shell already
   *  handed it (`useActivityFeed`'s own `MAX_TRACKED` ceiling), this only bounds the DOM. */
  max?: number;
}

export function ActivityFeedCard({ items, max = 20 }: ActivityFeedCardProps) {
  const visible = items.slice(0, max);

  return (
    <div
      style={{
        background: 'var(--surface-card)',
        border: '1px solid var(--border-default)',
        borderRadius: 'var(--radius-lg)',
        display: 'flex',
        flexDirection: 'column',
        minHeight: 220,
        gridColumn: 'span 1',
      }}
    >
      <div
        style={{
          padding: 'var(--space-3) var(--space-4)',
          borderBottom: '1px solid var(--border-subtle)',
          fontSize: 'var(--text-sm)',
          fontWeight: 600,
          color: 'var(--text-primary)',
        }}
      >
        Activity
      </div>
      <div style={{ flex: 1, overflowY: 'auto', maxHeight: 320 }}>
        {visible.length === 0 ? (
          <div
            style={{
              padding: 'var(--space-4)',
              textAlign: 'center',
              color: 'var(--text-tertiary)',
              fontSize: 'var(--text-sm)',
            }}
          >
            No recent activity
          </div>
        ) : (
          visible.map(item => (
            <div
              key={item.id}
              style={{
                display: 'flex',
                alignItems: 'baseline',
                gap: 'var(--space-2)',
                padding: 'var(--space-1) var(--space-4)',
                borderBottom: '1px solid var(--border-subtle)',
                fontFamily: 'var(--font-mono)',
                fontSize: 'var(--text-xs)',
              }}
            >
              <span style={{ color: LEVEL_COLOR[item.level], flexShrink: 0 }}>
                {item.level === 'ok' ? '[ok]' : item.level === 'warn' ? '[warn]' : '[error]'}
              </span>
              <span
                style={{
                  flex: 1,
                  minWidth: 0,
                  overflow: 'hidden',
                  textOverflow: 'ellipsis',
                  whiteSpace: 'nowrap',
                  color: 'var(--text-secondary)',
                }}
                title={item.text}
              >
                {item.text.replace(/^\[(ok|warn|error)\]\s*/, '')}
              </span>
              <span style={{ color: 'var(--text-tertiary)', flexShrink: 0 }}>{formatTime(item.ts)}</span>
            </div>
          ))
        )}
      </div>
    </div>
  );
}
