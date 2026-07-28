// LGUI-07 (spec §3.2, §5): the chat thread's message bubble. User bubbles align right,
// assistant bubbles align left and carry a small violet NodeBadge-style dot (spec's own
// phrasing — a plain colored dot styled like the NodeBadge kind-dot, not a full NodeBadge,
// since there's no node name/role to show here). Timestamps are mono, per brand.
//
// SAFE RENDER PATH (XSS proof, LGUI-07 AC): `content` is untrusted — it's whatever the
// assistant model produced, and could contain anything, including literal HTML like
// `<script>…</script>` (see the mock's `trigger:xss` fixture in aggregationClient.ts and the
// assertions in `../../lib/chatMarkdown.test.ts`). This component NEVER uses
// `dangerouslySetInnerHTML` anywhere. `parseBlocks`/`parseInline` (`../../lib/chatMarkdown.ts`)
// turn the raw string into a plain-data token list, and every branch below hands a token's
// string field to React as `{…}` JSX text content — React always escapes that as text, so even
// a literal `<script>` tag can only ever render as the visible characters `<script>`, never
// execute. There is no other rendering path for message content in this file.
import { parseBlocks } from '../../lib/chatMarkdown';
import type { InlineToken } from '../../lib/chatMarkdown';
import type { ChatMessage } from '../../hooks/useLuminaChat';

const LONG_REPLY_MAX_HEIGHT = 480;

function formatTimestamp(ts: number): string {
  const d = new Date(ts);
  return d.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit' });
}

function InlineContent({ tokens }: { tokens: InlineToken[] }) {
  return (
    <>
      {tokens.map((t, i) => {
        // Every branch renders a plain string as JSX text — see the file-level XSS note above.
        if (t.kind === 'bold') return <strong key={i}>{t.value}</strong>;
        if (t.kind === 'code') {
          return (
            <code
              key={i}
              style={{
                fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)',
                background: 'var(--space-700)', border: '1px solid var(--border)',
                borderRadius: 'var(--radius-md)', padding: '1px 5px',
              }}
            >
              {t.value}
            </code>
          );
        }
        if (t.kind === 'link') {
          return (
            <a
              key={i}
              href={t.href}
              target="_blank"
              rel="noreferrer noopener"
              style={{ color: 'var(--accent-bright)' }}
            >
              {t.label}
            </a>
          );
        }
        return <span key={i}>{t.value}</span>;
      })}
    </>
  );
}

function MessageBody({ content }: { content: string }) {
  const blocks = parseBlocks(content);
  return (
    <div style={{ whiteSpace: 'pre-wrap', wordBreak: 'break-word', lineHeight: 'var(--lh-relaxed, 1.5)' }}>
      {blocks.map((b, i) => {
        if (b.kind === 'codeblock') {
          return (
            <pre
              key={i}
              style={{
                margin: '6px 0', padding: 'var(--space-2)', overflowX: 'auto',
                background: 'var(--space-800, var(--space-700))', border: '1px solid var(--border)',
                borderRadius: 'var(--radius-md)', fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)',
              }}
            >
              {/* .value is plain text content, same guarantee as everywhere else in this file. */}
              {b.value}
            </pre>
          );
        }
        return <div key={i}><InlineContent tokens={b.tokens} /></div>;
      })}
    </div>
  );
}

export function ChatBubble({ message }: { message: ChatMessage }) {
  const isUser = message.role === 'user';
  return (
    <div
      style={{
        display: 'flex',
        justifyContent: isUser ? 'flex-end' : 'flex-start',
        marginBottom: 'var(--space-3)',
      }}
    >
      <div style={{ maxWidth: '72%', display: 'flex', flexDirection: 'column', gap: 4, alignItems: isUser ? 'flex-end' : 'flex-start' }}>
        {!isUser && (
          <span
            aria-hidden
            title="assistant"
            style={{
              width: 8, height: 8, borderRadius: '50%',
              background: 'var(--violet-400, var(--accent-bright))',
              boxShadow: '0 0 6px var(--violet-400, var(--accent-bright))',
              display: 'inline-block', marginBottom: 2,
            }}
          />
        )}
        <div
          style={{
            background: isUser ? 'var(--grad-card)' : 'var(--space-700)',
            border: `1px solid ${isUser ? 'var(--border-strong, var(--border))' : 'var(--border)'}`,
            borderRadius: 'var(--radius-lg)',
            padding: 'var(--space-3)',
            color: 'var(--text-100, var(--text-primary))',
            fontSize: 'var(--fs-md, 15px)',
            maxHeight: LONG_REPLY_MAX_HEIGHT,
            overflowY: 'auto',
          }}
        >
          <MessageBody content={message.content} />
        </div>
        <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-tertiary)' }}>
          {formatTimestamp(message.ts)}
        </span>
      </div>
    </div>
  );
}
