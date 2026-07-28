// LGUI-08 (§3.3): "Row → Drawer with the full Memory record ... provenance ... superseded_by
// link navigating the chain." No shared `Drawer` primitive exists in this repo yet (CONST-25/
// 26/27's shell modal/dialog kit — Drawer/Toast/CommandPalette — hasn't landed as of this
// build, confirmed by grep before writing this); same situation Muse's `ConfirmDialog.tsx` hit,
// and this follows its documented pattern: a minimal, brand-token, accessible right-side panel
// with an API (`open`/`memory`/`onClose`/`onNavigate`) a future shared `Drawer` can keep.
import { useEffect, useRef } from 'react';
import { MemoryTypeBadge } from './MemoryTypeBadge';
import { SensitivityBadge } from './SensitivityBadge';
import type { Memory } from '../../types/luminaMemory';

interface MemoryDrawerProps {
  memory: Memory | null;
  /** Looks up a record by id for the superseded-by chain link — `null` if not loaded/found
   *  (e.g. it fell outside the current filtered result set). */
  lookup: (id: string) => Memory | null;
  onClose: () => void;
  /** Navigates the drawer to a different record (superseded_by link). */
  onNavigate: (id: string) => void;
  /** Set while the panel is mid-navigate to a record not (yet) present in `lookup` — the search
   *  API has no fetch-by-id, so navigating clears filters and refetches; this renders a "still
   *  looking" state instead of silently closing the drawer while that refetch is in flight. */
  pendingId: string | null;
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div style={{ marginBottom: 'var(--space-3)' }}>
      <div style={{
        fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', textTransform: 'uppercase',
        letterSpacing: 'var(--ls-label)', color: 'var(--text-tertiary)', marginBottom: 4,
      }}>
        {label}
      </div>
      <div style={{ color: 'var(--text-primary)', fontSize: 'var(--fs-sm)' }}>{children}</div>
    </div>
  );
}

export function MemoryDrawer({ memory, lookup, onClose, onNavigate, pendingId }: MemoryDrawerProps) {
  const panelRef = useRef<HTMLDivElement>(null);
  const showPending = !memory && pendingId !== null;

  useEffect(() => {
    if (!memory && !showPending) return;
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose(); };
    window.addEventListener('keydown', onKey);
    panelRef.current?.focus();
    return () => window.removeEventListener('keydown', onKey);
  }, [memory, showPending, onClose]);

  if (!memory && !showPending) return null;

  if (!memory) {
    // Navigated to a superseded_by id outside the current (now-widened) result set. Stay open
    // with an honest "still resolving / not found" state instead of silently closing — closing
    // reads as a bug (per review finding), not as "there is no such record."
    return (
      <div
        role="presentation"
        onClick={onClose}
        style={{ position: 'fixed', inset: 0, background: 'rgba(13,11,26,0.55)', zIndex: 1000, display: 'flex', justifyContent: 'flex-end' }}
      >
        <div
          ref={panelRef}
          role="dialog"
          aria-modal="true"
          tabIndex={-1}
          onClick={e => e.stopPropagation()}
          style={{
            width: 'min(480px, 92vw)', height: '100%', padding: 'var(--space-5)',
            background: 'var(--grad-card)', borderLeft: '1px solid var(--border-strong)',
            boxShadow: 'var(--shadow-lg), var(--inset-hi)',
          }}
        >
          <div style={{ fontSize: 'var(--fs-h4)', fontWeight: 'var(--fw-semibold)', color: 'var(--text-100)', marginBottom: 'var(--space-3)' }}>
            Looking for record…
          </div>
          <p style={{ color: 'var(--text-tertiary)', fontSize: 'var(--fs-sm)' }}>
            Filters were cleared and the search re-run to look for <code style={{ fontFamily: 'var(--font-mono)' }}>{pendingId}</code>.
            If it still doesn't appear, the record may be outside the result limit or no longer exists.
          </p>
          <button
            type="button"
            onClick={onClose}
            style={{
              marginTop: 'var(--space-3)', background: 'transparent', border: '1px solid var(--border)',
              borderRadius: 'var(--radius-md)', color: 'var(--text-tertiary)', fontFamily: 'var(--font-mono)',
              fontSize: 'var(--fs-mono-sm)', padding: '5px 12px', cursor: 'pointer',
            }}
          >
            Close
          </button>
        </div>
      </div>
    );
  }

  const superseding = memory.superseded_by ? lookup(memory.superseded_by) : null;

  return (
    <div
      role="presentation"
      onClick={onClose}
      style={{ position: 'fixed', inset: 0, background: 'rgba(13,11,26,0.55)', zIndex: 1000, display: 'flex', justifyContent: 'flex-end' }}
    >
      <div
        ref={panelRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby="memory-drawer-title"
        tabIndex={-1}
        onClick={e => e.stopPropagation()}
        style={{
          width: 'min(480px, 92vw)', height: '100%', overflowY: 'auto',
          background: 'var(--grad-card)', borderLeft: '1px solid var(--border-strong)',
          boxShadow: 'var(--shadow-lg), var(--inset-hi)', padding: 'var(--space-5)',
        }}
      >
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'flex-start', marginBottom: 'var(--space-4)' }}>
          <div id="memory-drawer-title" style={{ fontSize: 'var(--fs-h4)', fontWeight: 'var(--fw-semibold)', color: 'var(--text-100)' }}>
            Memory record
          </div>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            style={{ background: 'transparent', border: 'none', color: 'var(--text-tertiary)', fontSize: 'var(--fs-h4)', cursor: 'pointer', lineHeight: 1 }}
          >
            ×
          </button>
        </div>

        <div style={{ display: 'flex', gap: 8, marginBottom: 'var(--space-4)' }}>
          <MemoryTypeBadge type={memory.memory_type} />
          <SensitivityBadge sensitivity={memory.sensitivity} />
        </div>

        <Field label="Content">
          <div style={{ whiteSpace: 'pre-wrap', lineHeight: 'var(--lh-body)' }}>{memory.content}</div>
        </Field>

        <Field label="Confidence">
          <span style={{ fontFamily: 'var(--font-mono)' }}>{memory.confidence.toFixed(2)}</span>
        </Field>

        <Field label="Visibility">{memory.visibility}</Field>

        <Field label="Created">
          <span style={{ fontFamily: 'var(--font-mono)' }}>{new Date(memory.created_at).toLocaleString()}</span>
        </Field>

        <Field label="Access count">
          <span style={{ fontFamily: 'var(--font-mono)' }}>{memory.access_count}</span>
        </Field>

        <Field label="User scope">{memory.user_id ?? 'shared / system'}</Field>

        <Field label="Provenance">
          {memory.provenance.conversation_id ? (
            <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)' }}>
              {memory.provenance.source} · conversation {memory.provenance.conversation_id}
              {memory.provenance.turn_index != null ? ` · turn ${memory.provenance.turn_index}` : ''}
            </span>
          ) : (
            <span style={{ color: 'var(--text-tertiary)' }}>{memory.provenance.source} (no conversation)</span>
          )}
        </Field>

        <Field label="Superseded by">
          {memory.superseded_by ? (
            <button
              type="button"
              onClick={() => onNavigate(memory.superseded_by!)}
              style={{
                background: 'transparent', border: '1px solid var(--border)', borderRadius: 'var(--radius-md)',
                color: 'var(--accent-bright)', fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)',
                padding: '3px 10px', cursor: 'pointer',
              }}
            >
              {superseding ? `→ ${memory.superseded_by} (${superseding.memory_type})` : `→ ${memory.superseded_by}`}
            </button>
          ) : (
            <span style={{ color: 'var(--text-tertiary)' }}>current — not superseded</span>
          )}
        </Field>
      </div>
    </div>
  );
}
