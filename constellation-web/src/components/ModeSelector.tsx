// MODE-05: Operating mode selector with routing detail panel.
// Replaces single segmented control with full 6-mode selector that also
// surfaces execution/review/triage routing read from GET /api/mode.
import { useState, useEffect, useCallback } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import { MODES } from '../types/presets';
import type { ModeId } from '../types/presets';
import { ModeDetail } from './ModeDetail';
import type { ModeRouting } from './ModeDetail';
import { RoleGate } from './RoleGate';

interface ApiModeResponse {
  mode: string;
  display_name: string;
  cost: string;
  limited: boolean;
  updated_at?: string;
  routing?: ModeRouting;
}

interface Props {
  /** Current mode from parent (e.g. from /api/status polling). */
  initialMode?: string;
}

export function ModeSelector({ initialMode = 'local' }: Props) {
  const [mode, setMode] = useState<ModeId>(
    (MODES.some(m => m.id === initialMode) ? initialMode : 'local') as ModeId
  );
  const [routing, setRouting] = useState<ModeRouting | null>(null);
  const [showDetail, setShowDetail] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState('');

  // Sync when parent updates initialMode (e.g. after polling /api/status)
  useEffect(() => {
    if (MODES.some(m => m.id === initialMode)) {
      setMode(initialMode as ModeId);
    }
  }, [initialMode]);

  // Fetch full routing detail from GET /api/mode
  const fetchRouting = useCallback(async () => {
    try {
      const data = await getAggregationClient().request<ApiModeResponse>('harmony', '/mode');
      if (data.routing) setRouting(data.routing);
      // Sync mode from server on initial load (in case initialMode is stale)
      if (MODES.some(m => m.id === data.mode)) {
        setMode(data.mode as ModeId);
      }
    } catch { /* best-effort */ }
  }, []);

  useEffect(() => { fetchRouting(); }, [fetchRouting]);

  const setModeApi = useCallback(async (newMode: ModeId) => {
    setSaving(true);
    setSaveError('');
    try {
      await getAggregationClient().request('harmony', '/mode', {
        method: 'PUT',
        body: JSON.stringify({ mode: newMode }),
      });
      setMode(newMode);
      // Refresh routing detail for the new mode
      await fetchRouting();
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : String(e));
    }
    setSaving(false);
  }, [fetchRouting]);

  const selected = MODES.find(m => m.id === mode) ?? MODES[0];

  return (
    <div className="h-card">
      <div
        className="h-card-header"
        style={{ cursor: 'default', flexDirection: 'column', alignItems: 'flex-start', gap: 8 }}
      >
        <span style={{ fontWeight: 600, fontSize: 'var(--text-md)' }}>Operating Mode</span>
        <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)' }}>
          {selected.label} · {selected.cost}
        </span>
      </div>

      <div className="h-card-body">
        {/* Mode buttons — segmented control. CONST-27: PUT /mode mutates, gated as a whole
            group (single RoleGate, not per-button) since a viewer never gets to change mode. */}
        <RoleGate display="block">
          <div style={{
            display: 'flex',
            gap: 4,
            marginBottom: 10,
            padding: 3,
            background: 'var(--bg-surface-raised)',
            borderRadius: 'var(--radius-lg)',
            border: '1px solid var(--border-default)',
            flexWrap: 'wrap',
          }}>
            {MODES.map(m => {
              const isActive = mode === m.id;
              return (
                <button
                  key={m.id}
                  onClick={() => setModeApi(m.id)}
                  disabled={saving}
                  title={`${m.label} — ${m.desc} (${m.cost})`}
                  style={{
                    flex: '1 1 auto',
                    minWidth: 58,
                    padding: '5px 4px',
                    fontSize: 'var(--text-xs)',
                    fontWeight: isActive ? 600 : 400,
                    border: 'none',
                    borderRadius: 'var(--radius-md)',
                    background: isActive ? 'var(--accent-primary)' : 'transparent',
                    color: isActive ? '#0a0a0a' : 'var(--text-secondary)',
                    cursor: saving ? 'wait' : 'pointer',
                    transition: `background var(--transition-fast), color var(--transition-fast)`,
                    lineHeight: 1.2,
                    whiteSpace: 'nowrap',
                  }}
                >
                  {m.label}
                </button>
              );
            })}
          </div>
        </RoleGate>

        {/* Active mode summary row with detail toggle */}
        <div style={{
          display: 'flex',
          alignItems: 'baseline',
          justifyContent: 'space-between',
          padding: 'var(--space-2) var(--space-3)',
          borderRadius: 'var(--radius-md)',
          background: 'var(--bg-surface-raised)',
          border: '1px solid var(--border-default)',
        }}>
          <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)' }}>
            {selected.desc}
          </span>
          <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginLeft: 8, flexShrink: 0 }}>
            <span style={{
              fontSize: 'var(--text-xs)',
              fontFamily: 'var(--font-mono)',
              color: 'var(--accent-primary)',
              fontWeight: 600,
            }}>
              {selected.cost}
            </span>
            {routing && (
              <button
                onClick={() => setShowDetail(d => !d)}
                title={showDetail ? 'Hide routing detail' : 'Show routing detail'}
                style={{
                  background: 'none',
                  border: 'none',
                  padding: '1px 4px',
                  cursor: 'pointer',
                  fontSize: 'var(--text-xs)',
                  color: showDetail ? 'var(--accent-primary)' : 'var(--text-tertiary)',
                  fontFamily: 'var(--font-mono)',
                  lineHeight: 1,
                }}
              >
                {showDetail ? '▲' : '▼'}
              </button>
            )}
          </div>
        </div>

        {/* Routing detail panel */}
        {showDetail && routing && (
          <div style={{
            marginTop: 8,
            padding: '8px 10px',
            borderRadius: 'var(--radius-md)',
            background: 'var(--bg-surface)',
            border: '1px solid var(--border-default)',
          }}>
            <ModeDetail routing={routing} />
          </div>
        )}

        {saving && (
          <div style={{ fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)', marginTop: 4 }}>
            saving…
          </div>
        )}
        {saveError && (
          <div style={{ fontSize: 'var(--text-xs)', color: 'var(--status-error)', marginTop: 4 }}>
            {saveError}
          </div>
        )}
      </div>
    </div>
  );
}
