// SPOL-08: Conversation bar — refined with design tokens and smooth expand/collapse.
// SGUI-02: Collapsible conversation bar
import { useState, useRef, useEffect, useCallback } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';

interface LogEntry { id: number; text: string; timestamp: string; }
interface Props { log: LogEntry[]; engineState: string; onAddLog?: (text: string) => void; }

export function ConversationBar({ log, engineState, onAddLog }: Props) {
  const [expanded, setExpanded] = useState(false);
  const [input, setInput] = useState('');
  const [sending, setSending] = useState(false);
  const [focused, setFocused] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!expanded) return;
    const handler = (e: MouseEvent) => {
      if (containerRef.current && !containerRef.current.contains(e.target as Node)) {
        setExpanded(false);
      }
    };
    document.addEventListener('mousedown', handler);
    return () => document.removeEventListener('mousedown', handler);
  }, [expanded]);

  useEffect(() => {
    if (expanded && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [log, expanded]);

  const lastEntry = log[log.length - 1];

  const sendCommand = useCallback(async () => {
    if (!input.trim() || sending) return;
    const cmd = input.trim();
    setSending(true);
    onAddLog?.(`> ${cmd}`);
    try {
      await getAggregationClient().request<{ ok: boolean; command: string }>('harmony', '/command', {
        method: 'POST',
        body: JSON.stringify({ command: cmd }),
      });
      setInput('');
    } catch (e) {
      onAddLog?.(`✗ ${e instanceof Error ? e.message : String(e)}`);
    }
    setSending(false);
  }, [input, sending, onAddLog]);

  const displayState = engineState === 'STOPPED' ? 'IDLE' : engineState;
  const stateColor = engineState === 'STOPPED'
    ? 'var(--text-tertiary)'
    : engineState.startsWith('EXECUTING')
      ? 'var(--status-success)'
      : 'var(--accent-primary)';

  return (
    <div
      ref={containerRef}
      style={{
        background: 'var(--bg-surface)',
        borderBottom: '1px solid var(--border-subtle)',
        flexShrink: 0,
      }}
    >
      {/* Collapsed strip — 40px height, full click target */}
      <div
        onClick={() => setExpanded(e => !e)}
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 'var(--space-2)',
          padding: 'var(--space-2) var(--space-4)',
          height: 40,
          cursor: 'pointer',
          userSelect: 'none',
          transition: `background var(--transition-fast)`,
        }}
        className="h-row"
      >
        <span style={{
          color: stateColor,
          fontSize: 'var(--text-xs)',
          fontWeight: 600,
          fontFamily: 'var(--font-mono)',
          flexShrink: 0,
          letterSpacing: '0.04em',
        }}>
          {displayState}
        </span>
        <span style={{
          fontFamily: 'var(--font-mono)',
          fontSize: 'var(--text-xs)',
          color: 'var(--text-secondary)',
          flex: 1,
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
        }}>
          {lastEntry?.text || 'Harmony ready.'}
        </span>
        {!expanded && (
          <span style={{
            color: 'var(--text-tertiary)',
            fontSize: 'var(--text-xs)',
            flexShrink: 0,
            fontStyle: 'italic',
          }}>
            click to talk
          </span>
        )}
        <span style={{
          color: 'var(--text-tertiary)',
          fontSize: 'var(--text-xs)',
          transform: expanded ? 'rotate(180deg)' : 'none',
          transition: `transform var(--transition-fast)`,
          flexShrink: 0,
          display: 'inline-block',
        }}>▼</span>
      </div>

      {/* Expanded panel */}
      {expanded && (
        <div style={{ borderTop: '1px solid var(--border-subtle)' }}>
          {/* Scrollable log */}
          <div
            ref={scrollRef}
            style={{
              maxHeight: 180,
              overflowY: 'auto',
              padding: 'var(--space-2) var(--space-4)',
              display: 'flex',
              flexDirection: 'column',
              gap: 2,
            }}
          >
            {log.slice(-200).map(entry => {
              const isSys = !entry.text.startsWith('>');
              return (
                <div key={entry.id} style={{
                  display: 'flex',
                  gap: 'var(--space-2)',
                  fontFamily: 'var(--font-mono)',
                  fontSize: 'var(--text-xs)',
                  lineHeight: 1.5,
                }}>
                  <span style={{ color: 'var(--text-tertiary)', flexShrink: 0 }}>
                    {entry.timestamp}
                  </span>
                  <span style={{ color: isSys ? 'var(--text-secondary)' : 'var(--text-primary)' }}>
                    {entry.text}
                  </span>
                </div>
              );
            })}
          </div>

          {/* Command input row */}
          <div style={{
            display: 'flex',
            gap: 'var(--space-2)',
            padding: 'var(--space-2) var(--space-4)',
            borderTop: '1px solid var(--border-subtle)',
          }}>
            <input
              type="text"
              value={input}
              onChange={e => setInput(e.target.value)}
              onKeyDown={e => { if (e.key === 'Enter') sendCommand(); }}
              onFocus={() => setFocused(true)}
              onBlur={() => setFocused(false)}
              placeholder="Type a command or ask a question…"
              autoFocus
              style={{
                flex: 1,
                background: 'var(--bg-surface-raised)',
                border: `1px solid ${focused ? 'var(--accent-primary)' : 'var(--border-default)'}`,
                borderRadius: 'var(--radius-md)',
                color: 'var(--text-primary)',
                padding: 'var(--space-1) var(--space-3)',
                fontSize: 'var(--text-sm)',
                fontFamily: 'var(--font-mono)',
                outline: 'none',
                transition: `border-color var(--transition-fast)`,
              }}
            />
            <button
              className="h-btn h-btn-teal"
              onClick={sendCommand}
              disabled={sending || !input.trim()}
              style={{ padding: 'var(--space-1) var(--space-3)', fontSize: 'var(--text-xs)' }}
            >
              {sending ? '…' : 'Send'}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
