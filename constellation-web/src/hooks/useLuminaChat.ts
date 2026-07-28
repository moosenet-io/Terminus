// LGUI-07: state + send logic for the Lumina Conversations panel (spec §3.2). Single
// conversation, v1 — there is no history-list API (honest scope, see the spec's own note),
// so this hook only ever holds the in-memory thread for the current tab session.
//
// The wire call is the pre-existing chat endpoint (spec §0.2), reached through the Terminus
// proxy at `/api/lumina/v1/chat/completions` — OpenAI-shaped request/response, explicitly
// NON-STREAMING. This hook never simulates token-by-token output; the composer just disables
// with a "thinking" state for the one round trip.
import { useCallback, useEffect, useRef, useState } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';

/** Best-effort parse of a fetch Response body as a chat-completions error envelope. Used only
 *  on the transport-error path (`httpJson` already threw on non-2xx and discarded the body), so
 *  this re-fetches nothing — it just gives the catch block a chance to classify by HTTP status
 *  when the server didn't (or couldn't) hand back a parseable `{error:{type,message}}` body. A
 *  502 always classifies as upstream; a 429 always classifies as rate_limit; anything else is
 *  'other'. This keeps the mapping correct against the real backend (spec §0.2: 429/502) even
 *  though the aggregation client's generic `request<T>()` doesn't surface the response body on
 *  a thrown HTTP error. */
function classifyHttpErrorMessage(message: string): ChatErrorKind {
  const statusMatch = /^HTTP (\d+) /.exec(message);
  const status = statusMatch ? Number(statusMatch[1]) : null;
  if (status === 429) return 'rate_limit';
  if (status === 502 || status === 503 || status === 504) return 'upstream';
  // A recognized HTTP status that isn't the rate-limit/upstream pair (400/409/…) is a real
  // "anything else" per spec §3.2 — the generic inline+retry path, not "Chord unreachable".
  // Only the absence of an "HTTP NNN" prefix at all (a thrown network/TypeError with no status)
  // is genuinely unreachable-transport, which is the sole case that reads as upstream.
  if (status !== null) return 'other';
  return 'upstream';
}

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
  // to remember what was last typed. `messageId` lets retry() remove the FAILED user bubble
  // before resending, so a retry replaces the failed turn instead of duplicating it in both the
  // visible transcript and the history sent to the model.
  const lastAttempt = useRef<{ content: string; messageId: string } | null>(null);
  // Mirrors `messages`, updated synchronously (not via the React state-commit cycle), so
  // sendRaw's history build and retry's "drop the failed bubble" both see the same-tick truth
  // instead of a closure captured before a same-tick setMessages() call has flushed.
  const messagesRef = useRef<ChatMessage[]>([]);
  useEffect(() => { messagesRef.current = messages; }, [messages]);
  // The `thinking` STATE only reflects reality after React commits a render — two retry()
  // clicks fired before that commit both read `thinking === false` and would both proceed
  // concurrently. This ref is set/checked synchronously in the same tick, so the second call
  // is rejected immediately regardless of render timing.
  const inFlightRef = useRef(false);

  const sendRaw = useCallback(async (content: string, replaceMessageId?: string) => {
    const trimmed = content.trim();
    if (!trimmed || inFlightRef.current) return;
    inFlightRef.current = true;

    const userMsg: ChatMessage = { id: nextId(), role: 'user', content: trimmed, ts: Date.now() };
    const base = replaceMessageId
      ? messagesRef.current.filter(m => m.id !== replaceMessageId)
      : messagesRef.current;
    const nextMessages = [...base, userMsg];
    messagesRef.current = nextMessages;
    setError(null);
    setMessages(nextMessages);
    setThinking(true);
    lastAttempt.current = { content: trimmed, messageId: userMsg.id };

    try {
      const history = nextMessages.map(m => ({ role: m.role, content: m.content }));
      const res = await getAggregationClient().request<ChatCompletionResponse | ChatCompletionErrorEnvelope>(
        'lumina',
        '/v1/chat/completions',
        { method: 'POST', body: JSON.stringify({ messages: history }) },
      );

      if (isErrorEnvelope(res)) {
        setError({ kind: classifyErrorType(res.error.type), message: res.error.message });
        return;
      }

      // CONST-28-style degrade: an unconfigured/unreachable proxy can come back HTTP 200 with
      // `{available:false, detail}` rather than an error envelope or a 4xx/5xx (LGUI-05's proxy
      // wiring). Treat that the same as an upstream failure instead of silently appending an
      // empty assistant bubble.
      if ('available' in res && (res as { available?: boolean }).available === false) {
        const detail = (res as { detail?: string }).detail;
        setError({ kind: 'upstream', message: detail ?? 'Chord unreachable' });
        return;
      }

      const replyContent = res.choices?.[0]?.message?.content ?? '';
      const assistantMsg: ChatMessage = {
        id: nextId(), role: 'assistant', content: replyContent, ts: Date.now(),
      };
      setMessages(prev => [...prev, assistantMsg]);
    } catch (e) {
      // A thrown error here means the transport itself failed. `httpJson` throws
      // `Error("HTTP {status} for {path}")` on any non-2xx WITHOUT reading the body, so the
      // spec's 429 (rate_limit_error) / 502 (upstream_error) distinction has to be recovered
      // from the status embedded in the message rather than a parsed envelope — a genuine
      // network failure (no "HTTP NNN" prefix) still reads as upstream-unreachable.
      const message = e instanceof Error ? e.message : 'Chord unreachable';
      setError({ kind: classifyHttpErrorMessage(message), message });
    } finally {
      inFlightRef.current = false;
      setThinking(false);
    }
  }, []);

  const send = useCallback((rawText: string, override: RouterOverride) => {
    const content = override ? `/${override} ${rawText}` : rawText;
    return sendRaw(content);
  }, [sendRaw]);

  const retry = useCallback(() => {
    const attempt = lastAttempt.current;
    if (!attempt) return;
    // Drop the failed user bubble and resend in the SAME sendRaw call (via replaceMessageId) —
    // both the base array sendRaw builds history from and the setMessages() it issues reflect
    // the filtered list atomically, so a retry replaces the failed turn instead of duplicating
    // it in the visible transcript and the history sent to the model.
    void sendRaw(attempt.content, attempt.messageId);
  }, [sendRaw]);

  return { messages, thinking, error, send, retry };
}
