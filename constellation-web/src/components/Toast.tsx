// CONST-26 (§3.3): toast notifications for mutation results and health transitions ONLY --
// nothing else in the app pushes a toast. Two integration points:
//   - Mutation results: observed centrally via `aggregationClient.onMutationResult` (fired by
//     every mutating `request<T>()` call, from every panel, with no per-panel opt-in needed --
//     see that file's "single path to the backend" seam doc).
//   - Health transitions: pushed explicitly by `panels/overview/ActivityFeedCard.tsx`, the one
//     place that already diffs consecutive `/api/health` polls.
// Auto-dismiss after 6s; `aria-live="polite"` so a screen reader announces new toasts without
// interrupting whatever the user is doing. Nothing here ever touches localStorage/sessionStorage
// (the CONST-16 prefs seam is layout/density only -- toasts are ephemeral, in-memory-only, by
// design).
import { createContext, useCallback, useContext, useEffect, useState } from 'react';
import type { ReactNode } from 'react';
import { onMutationResult } from '../lib/aggregationClient';

export type ToastLevel = 'ok' | 'warn' | 'error';

export interface ToastMessage {
  id: string;
  text: string;
  level: ToastLevel;
}

interface ToastContextValue {
  push: (text: string, level?: ToastLevel) => void;
}

const ToastContext = createContext<ToastContextValue | null>(null);

const AUTO_DISMISS_MS = 6000;

let toastSeq = 0;
function nextToastId(): string {
  toastSeq += 1;
  return `toast-${toastSeq}-${Date.now()}`;
}

/** Mount ONCE, near the app root (see `App.tsx`) -- wraps the whole tree so any component below
 *  it can `useToastContext()` to push a toast, and renders the fixed-position toast stack. */
export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<ToastMessage[]>([]);

  const dismiss = useCallback((id: string) => {
    setToasts(prev => prev.filter(t => t.id !== id));
  }, []);

  const push = useCallback((text: string, level: ToastLevel = 'ok') => {
    const id = nextToastId();
    setToasts(prev => [...prev, { id, text, level }]);
    // Scheduled once per toast at push time (not in a `useEffect` keyed on the whole `toasts`
    // array) so an already-showing toast's own countdown is never restarted just because a
    // second toast arrived alongside it.
    setTimeout(() => dismiss(id), AUTO_DISMISS_MS);
  }, [dismiss]);

  useEffect(() => onMutationResult(event => {
    const label = `${event.method} ${event.path}`;
    if (event.ok) {
      push(label, 'ok');
    } else {
      push(`${label} failed${event.error ? `: ${event.error}` : ''}`, 'error');
    }
  }), [push]);

  return (
    <ToastContext.Provider value={{ push }}>
      {children}
      <ToastStack toasts={toasts} onDismiss={dismiss} />
    </ToastContext.Provider>
  );
}

/** Push a toast from anywhere under `ToastProvider` -- e.g. `ActivityFeedCard` calls this for
 *  health transitions. Throws outside a provider (a missing provider is a wiring bug, not a
 *  degrade-silently case). */
export function useToastContext(): ToastContextValue {
  const ctx = useContext(ToastContext);
  if (!ctx) {
    throw new Error('useToastContext() must be used within a <ToastProvider>');
  }
  return ctx;
}

const LEVEL_COLOR: Record<ToastLevel, string> = {
  ok: 'var(--status-success)',
  warn: 'var(--status-warning)',
  error: 'var(--status-error)',
};

function ToastStack({ toasts, onDismiss }: { toasts: ToastMessage[]; onDismiss: (id: string) => void }) {
  if (toasts.length === 0) return null;
  return (
    <div
      aria-live="polite"
      role="status"
      style={{
        position: 'fixed',
        bottom: 'var(--space-5)',
        right: 'var(--space-5)',
        display: 'flex',
        flexDirection: 'column',
        gap: 'var(--space-2)',
        zIndex: 1000,
        maxWidth: 360,
      }}
    >
      {toasts.map(t => (
        <div
          key={t.id}
          style={{
            display: 'flex',
            alignItems: 'flex-start',
            justifyContent: 'space-between',
            gap: 'var(--space-3)',
            background: 'var(--bg-surface-raised)',
            border: `1px solid ${LEVEL_COLOR[t.level]}`,
            borderRadius: 'var(--radius-md)',
            padding: 'var(--space-2) var(--space-3)',
            color: 'var(--text-primary)',
            fontSize: 'var(--text-sm)',
            fontFamily: 'var(--font-mono)',
            boxShadow: '0 4px 16px rgba(0,0,0,0.4)',
          }}
        >
          <span style={{ wordBreak: 'break-word' }}>{t.text}</span>
          <button
            onClick={() => onDismiss(t.id)}
            aria-label="Dismiss notification"
            style={{
              background: 'transparent',
              border: 'none',
              color: 'var(--text-tertiary)',
              cursor: 'pointer',
              fontSize: 'var(--text-sm)',
              lineHeight: 1,
              padding: 0,
            }}
          >
            ×
          </button>
        </div>
      ))}
    </div>
  );
}
