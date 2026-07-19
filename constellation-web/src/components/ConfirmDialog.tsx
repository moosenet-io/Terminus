// CONST-20 SEAM (temporary, clearly marked): no shared `ConfirmDialog` exists in this repo yet
// -- §2.3 lists it as a future restyle target, but the item that actually builds the shell's
// modal/dialog kit (alongside Drawer/Toast/CommandPalette) is CONST-25/26/27, none merged as
// of this build. Muse's channel compose/maintenance actions need one now (spec §5.4: "each
// opens ConfirmDialog"), so this is a minimal, brand-token, accessible stand-in with the API a
// future shared component can keep unchanged: `open`/`title`/`description`/`onConfirm`/
// `onCancel`. role="dialog" + aria-modal + Esc-to-cancel + initial focus per §2.6's
// accessibility floor ("role=\"dialog\" + focus trap on palette/drawer/confirm").
import { useEffect, useRef } from 'react';
import { Button } from './Button';

interface ConfirmDialogProps {
  open: boolean;
  title: string;
  description?: string;
  confirmLabel?: string;
  cancelLabel?: string;
  /** Renders the confirm action as the `danger` Button variant instead of `primary`. */
  destructive?: boolean;
  /** Disables both actions and swaps the confirm label to a busy state (async mutation in flight). */
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}

export function ConfirmDialog({
  open,
  title,
  description,
  confirmLabel = 'Confirm',
  cancelLabel = 'Cancel',
  destructive = false,
  busy = false,
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  const dialogRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onCancel();
    };
    window.addEventListener('keydown', onKey);
    dialogRef.current?.focus();
    return () => window.removeEventListener('keydown', onKey);
  }, [open, onCancel]);

  if (!open) return null;

  return (
    <div
      role="presentation"
      onClick={onCancel}
      style={{
        position: 'fixed',
        inset: 0,
        background: 'rgba(13,11,26,0.65)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        zIndex: 1000,
      }}
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby="confirm-dialog-title"
        tabIndex={-1}
        onClick={e => e.stopPropagation()}
        style={{
          background: 'var(--grad-card)',
          border: '1px solid var(--border-strong)',
          borderRadius: 'var(--radius-lg)',
          boxShadow: 'var(--shadow-lg), var(--inset-hi)',
          padding: 'var(--space-5)',
          maxWidth: 420,
          width: '90%',
        }}
      >
        <div
          id="confirm-dialog-title"
          style={{ fontSize: 'var(--fs-h4)', fontWeight: 'var(--fw-semibold)', color: 'var(--text-100)', marginBottom: 'var(--space-2)' }}
        >
          {title}
        </div>
        {description && (
          <div style={{ fontSize: 'var(--fs-body)', color: 'var(--text-body)', marginBottom: 'var(--space-4)', lineHeight: 'var(--lh-body)' }}>
            {description}
          </div>
        )}
        <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 'var(--space-2)' }}>
          <Button variant="ghost" size="sm" onClick={onCancel} disabled={busy}>
            {cancelLabel}
          </Button>
          <Button variant={destructive ? 'danger' : 'primary'} size="sm" onClick={onConfirm} disabled={busy}>
            {busy ? 'Working…' : confirmLabel}
          </Button>
        </div>
      </div>
    </div>
  );
}
