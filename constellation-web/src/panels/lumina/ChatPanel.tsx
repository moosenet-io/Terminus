// LGUI-07: Conversations panel (spec §3.2, route `/lumina/chat`, min role operator per §2's
// panel table). Single-conversation view v1 — honest scope, no history-list API exists yet.
import { useCallback, useEffect, useRef, useState } from 'react';
import type { KeyboardEvent } from 'react';
import { Card, CardTitle } from '../../components/Card';
import { StatusPill } from '../../components/StatusPill';
import { useAuthRole } from '../../hooks/AuthRoleContext';
import { useLuminaChat, SESSION_IDLE_MS } from '../../hooks/useLuminaChat';
import type { RouterOverride } from '../../hooks/useLuminaChat';
import { ChatBubble } from './ChatBubble';

const ERROR_COPY: Record<'rate_limit' | 'upstream', string> = {
  rate_limit: 'Daily turn budget reached',
  upstream: 'Chord unreachable',
};

/** §3.2: "session resumes · 30 min idle" divider whenever the gap between two consecutive
 *  messages' client-side timestamps exceeds the idle window. */
function IdleDivider() {
  return (
    <div
      role="separator"
      style={{
        display: 'flex', alignItems: 'center', gap: 8, margin: 'var(--space-3) 0',
        color: 'var(--text-tertiary)', fontSize: 'var(--fs-mono-sm)', fontFamily: 'var(--font-mono)',
        textTransform: 'uppercase', letterSpacing: 'var(--ls-label)',
      }}
    >
      <span style={{ flex: 1, height: 1, background: 'var(--line-soft, var(--border))' }} />
      session resumes · 30 min idle
      <span style={{ flex: 1, height: 1, background: 'var(--line-soft, var(--border))' }} />
    </div>
  );
}

/** Read-only placeholder a viewer session sees (spec §3.2 edge case). Cosmetic mirror of the
 *  server-side role gate — the server never routes a viewer to the mutating chat endpoint in
 *  the first place (same convention as `RoleGate`, see its doc comment). */
function ViewerPlaceholder() {
  return (
    <Card variant="content">
      <CardTitle subtitle="Conversations require operator access">Read-only transcript</CardTitle>
      <p style={{ color: 'var(--text-tertiary)', fontSize: 'var(--fs-sm)', margin: 0 }}>
        Your session can view Lumina's status elsewhere in this module, but starting or
        continuing a conversation needs operator role. Ask an operator to grant access if you
        need to chat with the assistant directly.
      </p>
    </Card>
  );
}

function ComposerChip({ active, label, onClick }: { active: boolean; label: string; onClick: () => void }) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      style={{
        fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', textTransform: 'uppercase',
        letterSpacing: 'var(--ls-label)', padding: '3px 10px', borderRadius: 'var(--radius-pill)',
        border: `1px solid ${active ? 'var(--accent-bright)' : 'var(--border)'}`,
        background: active ? 'color-mix(in srgb, var(--accent-bright) 18%, var(--space-700))' : 'var(--space-700)',
        color: active ? 'var(--accent-bright)' : 'var(--text-tertiary)',
        cursor: 'pointer',
      }}
    >
      {label}
    </button>
  );
}

export function ChatPanel() {
  const role = useAuthRole();
  const { messages, thinking, error, send, retry } = useLuminaChat();
  const [draft, setDraft] = useState('');
  const [override, setOverride] = useState<RouterOverride>(null);
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
  }, [messages.length, thinking]);

  const submit = useCallback(() => {
    if (!draft.trim() || thinking) return;
    void send(draft, override);
    setDraft('');
    setOverride(null);
  }, [draft, override, thinking, send]);

  const onKeyDown = useCallback((e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      submit();
    }
  }, [submit]);

  if (role === 'viewer') {
    return (
      <div style={{ padding: 'var(--space-5)' }}>
        <CardTitle subtitle="/lumina/chat">Conversations</CardTitle>
        <ViewerPlaceholder />
      </div>
    );
  }

  return (
    <div style={{ padding: 'var(--space-5)', height: '100%', display: 'flex', flexDirection: 'column', gap: 'var(--space-3)' }}>
      <CardTitle subtitle="Talk to the assistant — non-streaming, one turn at a time">Conversations</CardTitle>

      <Card variant="content" style={{ flex: 1, display: 'flex', flexDirection: 'column', minHeight: 0 }}>
        <div ref={scrollRef} style={{ flex: 1, overflowY: 'auto', padding: 'var(--space-2)' }}>
          {messages.length === 0 && !thinking && (
            <div style={{ color: 'var(--text-tertiary)', fontSize: 'var(--fs-sm)', textAlign: 'center', marginTop: 'var(--space-5)' }}>
              No messages yet — say something below to start the conversation.
            </div>
          )}
          {messages.map((m, i) => {
            const prev = messages[i - 1];
            const showDivider = prev != null && m.ts - prev.ts > SESSION_IDLE_MS;
            return (
              <div key={m.id}>
                {showDivider && <IdleDivider />}
                <ChatBubble message={m} />
              </div>
            );
          })}
          {thinking && (
            <div style={{ display: 'flex', justifyContent: 'flex-start', marginBottom: 'var(--space-3)' }}>
              <StatusPill state="idle" label="thinking" />
            </div>
          )}
        </div>

        {error && (
          <div style={{ padding: '0 var(--space-3) var(--space-2)' }}>
            {error.kind === 'rate_limit' || error.kind === 'upstream' ? (
              <Card variant="content" style={{ borderColor: error.kind === 'rate_limit' ? 'var(--flux-amber)' : undefined }}>
                <span style={{
                  color: error.kind === 'rate_limit' ? 'var(--flux-amber)' : 'var(--status-error)',
                  fontSize: 'var(--fs-sm)',
                }}>
                  {ERROR_COPY[error.kind]}
                </span>
              </Card>
            ) : (
              <div style={{ display: 'flex', alignItems: 'center', gap: 8, color: 'var(--status-error)', fontSize: 'var(--fs-sm)' }}>
                <span>{error.message || 'Something went wrong.'}</span>
                <button
                  type="button"
                  onClick={retry}
                  style={{
                    fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--accent-bright)',
                    background: 'transparent', border: '1px solid var(--border)', borderRadius: 'var(--radius-md)',
                    padding: '2px 8px', cursor: 'pointer',
                  }}
                >
                  retry
                </button>
              </div>
            )}
          </div>
        )}

        <div style={{ borderTop: '1px solid var(--border)', padding: 'var(--space-3)', display: 'flex', flexDirection: 'column', gap: 8 }}>
          <div style={{ display: 'flex', gap: 8 }}>
            <ComposerChip active={override === 'deep'} label="/deep" onClick={() => setOverride(o => (o === 'deep' ? null : 'deep'))} />
            <ComposerChip active={override === 'quick'} label="/quick" onClick={() => setOverride(o => (o === 'quick' ? null : 'quick'))} />
          </div>
          <textarea
            value={draft}
            onChange={e => setDraft(e.target.value)}
            onKeyDown={onKeyDown}
            disabled={thinking}
            placeholder="Message Lumina… (Enter to send, Shift+Enter for a newline)"
            rows={3}
            style={{
              width: '100%', resize: 'vertical', background: 'var(--space-700)',
              border: '1px solid var(--border)', borderRadius: 'var(--radius-md)',
              color: 'var(--text-primary)', padding: 'var(--space-2)', fontSize: 'var(--fs-sm)',
              fontFamily: 'inherit', outline: 'none', opacity: thinking ? 0.6 : 1,
            }}
          />
          <div style={{ display: 'flex', justifyContent: 'flex-end' }}>
            <button
              type="button"
              onClick={submit}
              disabled={thinking || !draft.trim()}
              style={{
                fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-sm)', padding: '6px 16px',
                borderRadius: 'var(--radius-md)', border: '1px solid var(--accent-bright)',
                background: thinking || !draft.trim() ? 'var(--space-700)' : 'var(--accent-bright)',
                color: thinking || !draft.trim() ? 'var(--text-tertiary)' : 'var(--space-900, #0D0B1A)',
                cursor: thinking || !draft.trim() ? 'not-allowed' : 'pointer',
              }}
            >
              {thinking ? 'Sending…' : 'Send'}
            </button>
          </div>
        </div>
      </Card>
    </div>
  );
}
