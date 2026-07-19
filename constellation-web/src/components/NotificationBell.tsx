// CONST-26 (§3.3): the status-strip bell menu. Retains the last 50 feed items IN MEMORY ONLY --
// no localStorage/sessionStorage (the CONST-16 prefs seam is layout/density only; a notification
// history is not UI preference state and must not persist across reloads per spec). Reads from
// the SAME `FeedItem[]` the Overview `ActivityFeedCard` renders (owned by the shell's
// `useActivityFeed` hook) -- this component is a pure renderer + open/close toggle, no polling
// of its own.
import { useState } from 'react';
import type { FeedItem } from '../lib/activityFeed';
import { capFeed } from '../lib/activityFeed';

const BELL_CAP = 50;

const LEVEL_COLOR: Record<FeedItem['level'], string> = {
  ok: 'var(--status-success)',
  warn: 'var(--status-warning)',
  error: 'var(--status-error)',
};

interface NotificationBellProps {
  items: FeedItem[];
}

export function NotificationBell({ items }: NotificationBellProps) {
  const [open, setOpen] = useState(false);
  const recent = capFeed(items, BELL_CAP);

  return (
    <div style={{ position: 'relative' }}>
      <button
        onClick={() => setOpen(o => !o)}
        aria-label="Notifications"
        aria-expanded={open}
        style={{
          background: 'transparent',
          border: 'none',
          color: 'var(--text-secondary)',
          cursor: 'pointer',
          padding: 'var(--space-1) var(--space-2)',
          display: 'flex',
          alignItems: 'center',
          gap: 4,
          fontSize: 'var(--text-sm)',
        }}
      >
        <span aria-hidden="true">🔔</span>
        {recent.length > 0 && (
          <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)', fontFamily: 'var(--font-mono)' }}>
            {recent.length}
          </span>
        )}
      </button>

      {open && (
        <div
          role="menu"
          style={{
            position: 'absolute',
            right: 0,
            top: '100%',
            marginTop: 'var(--space-1)',
            width: 340,
            maxHeight: 420,
            overflowY: 'auto',
            background: 'var(--bg-surface-raised)',
            border: '1px solid var(--border-default)',
            borderRadius: 'var(--radius-md)',
            boxShadow: '0 8px 24px rgba(0,0,0,0.5)',
            zIndex: 1000,
          }}
        >
          {recent.length === 0 ? (
            <div style={{ padding: 'var(--space-3)', color: 'var(--text-tertiary)', fontSize: 'var(--text-sm)' }}>
              No recent activity
            </div>
          ) : (
            recent.map(item => (
              <div
                key={item.id}
                style={{
                  padding: 'var(--space-2) var(--space-3)',
                  borderBottom: '1px solid var(--border-subtle)',
                  fontSize: 'var(--text-xs)',
                  fontFamily: 'var(--font-mono)',
                  color: 'var(--text-secondary)',
                  display: 'flex',
                  gap: 'var(--space-2)',
                }}
              >
                <span style={{ color: LEVEL_COLOR[item.level], flexShrink: 0 }}>
                  {item.level === 'ok' ? '[ok]' : item.level === 'warn' ? '[warn]' : '[error]'}
                </span>
                <span style={{ flex: 1, minWidth: 0, overflowWrap: 'break-word' }}>
                  {item.text.replace(/^\[(ok|warn|error)\]\s*/, '')}
                </span>
              </div>
            ))
          )}
        </div>
      )}
    </div>
  );
}
