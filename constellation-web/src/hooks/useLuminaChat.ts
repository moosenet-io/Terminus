// LGUI-07: state + send logic for the Lumina Conversations panel (spec §3.2). Single
// conversation, v1 — there is no history-list API (honest scope, see the spec's own note),
// so this hook only ever holds the in-memory thread for the current tab session.
//
// The wire call is the pre-existing chat endpoint (spec §0.2), reached through the Terminus
// proxy at `/api/lumina/v1/chat/completions` — OpenAI-shaped request/response, explicitly
// NON-STREAMING. This hook never simulates token-by-token output; the composer just disables
// with a "thinking" state for the one round trip.
import { useCallback, useRef, useState } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';

export type ChatRole = 'user' | 'assistant';

export interface ChatMessage {
  id: string;
  role: ChatRole;
  content: string;
  ts: number;
}

/** The three error shapes the panel distinguishes per spec §3.2. `'other'` covers any
 *  `error.type` value that isn't one of the two named ones — still shown inline, with a retry
 *  affordance, per the spec's "anything else" clause. */
export type ChatErrorKind = 'rate_limit' | 'upstream' | 'other';

export interface ChatError {
  kind: ChatErrorKind;
  message: string;
}

/** REAL router overrides (spec §3.2/§0.1.4) — prefixed onto the outgoing message content,
 *  exactly like a user typing `/deep `/`/quick ` themselves. Not a client-side routing
 *  decision; the prefix is what the router on the other end actually keys off of. */
export type RouterOverride = 'deep' | 'quick' | null;

interface ChatCompletionChoice {
  message?: { role?: string; content?: string };
}

interface ChatCompletionResponse {
  choices?: ChatCompletionChoice[];
}

interface ChatCompletionErrorEnvelope {
  error: { message: string; type: string };
}

function isErrorEnvelope(x: unknown): x is ChatCompletionErrorEnvelope {
  return typeof x === 'object' && x !== null && 'error' in x
    && typeof (x as { error?: unknown }).error === 'object' && (x as { error?: unknown }).error !== null;
}

function classifyErrorType(type: string): ChatErrorKind {
  if (type === 'rate_limit_error') return 'rate_limit';
  if (type === 'upstream_error') return 'upstream';
  return 'other';
}

let idCounter = 0;
function nextId(): string {
  idCounter += 1;
  return `msg-${Date.now()}-${idCounter}`;
}

/** Idle window per spec §3.2 (`LUMINA_CONV_BUFFER_*` semantics) — client-side only, purely
 *  cosmetic (a divider), never gates the actual request. */
export const SESSION_IDLE_MS = 30 * 60 * 1000;

export function useLuminaChat() {
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [thinking, setThinking] = useState(false);
  const [error, setError] = useState<ChatError | null>(null);
  // Kept so the "anything else" error's retry affordance can resend without the caller having
  // to remember what was last typed.
  const lastAttempt = useRef<{ content: string } | null>(null);

  const sendRaw = useCallback(async (content: string) => {
    const trimmed = content.trim();
    if (!trimmed || thinking) return;

    const userMsg: ChatMessage = { id: nextId(), role: 'user', content: trimmed, ts: Date.now() };
    setError(null);
    setMessages(prev => [...prev, userMsg]);
    setThinking(true);
    lastAttempt.current = { content: trimmed };

    try {
      const history = [...messages, userMsg].map(m => ({ role: m.role, content: m.content }));
      const res = await getAggregationClient().request<ChatCompletionResponse | ChatCompletionErrorEnvelope>(
        'lumina',
        '/v1/chat/completions',
        { method: 'POST', body: JSON.stringify({ messages: history }) },
      );

      if (isErrorEnvelope(res)) {
        setError({ kind: classifyErrorType(res.error.type), message: res.error.message });
        return;
      }

      const replyContent = res.choices?.[0]?.message?.content ?? '';
      const assistantMsg: ChatMessage = {
        id: nextId(), role: 'assistant', content: replyContent, ts: Date.now(),
      };
      setMessages(prev => [...prev, assistantMsg]);
    } catch (e) {
      // A thrown error here means the transport itself failed (non-2xx with no parseable
      // envelope, network failure, …) — the honest read is "the proxy/upstream is unreachable",
      // same degraded-card treatment as an explicit upstream_error envelope.
      setError({ kind: 'upstream', message: e instanceof Error ? e.message : 'Chord unreachable' });
    } finally {
      setThinking(false);
    }
  }, [messages, thinking]);

  const send = useCallback((rawText: string, override: RouterOverride) => {
    const content = override ? `/${override} ${rawText}` : rawText;
    return sendRaw(content);
  }, [sendRaw]);

  const retry = useCallback(() => {
    if (lastAttempt.current) {
      void sendRaw(lastAttempt.current.content);
    }
  }, [sendRaw]);

  return { messages, thinking, error, send, retry };
}
