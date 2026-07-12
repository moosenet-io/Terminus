// ROUTE-03 + CCTX-07: 10-notch routing preset slider with compression level toggle.
import { useState, useCallback, useEffect } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import { PRESETS } from '../types/presets';

type CompressionLevel = 'off' | 'light' | 'moderate';

interface Props {
  /** Current preset notch (1-10). Defaults to 1 if not provided. */
  initialValue?: number;
}

export function InferenceMixSlider({ initialValue = 1 }: Props) {
  const [notch, setNotch] = useState<number>(Math.max(1, Math.min(10, initialValue)));
  const [limited, setLimited] = useState(false);
  const [compression, setCompression] = useState<CompressionLevel>('moderate');
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState('');

  // Sync initialValue changes (e.g. after API response loads)
  useEffect(() => {
    if (initialValue >= 1 && initialValue <= 10) {
      setNotch(initialValue);
    }
  }, [initialValue]);

  const preset = PRESETS[notch - 1];

  const postPreset = useCallback(async (newNotch: number, newLimited: boolean) => {
    setSaving(true);
    setSaveError('');
    try {
      await getAggregationClient().request('harmony', '/commands/inference-mix', {
        method: 'POST',
        body: JSON.stringify({ preset: newNotch, limited: newLimited }),
      });
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : String(e));
    }
    setSaving(false);
  }, []);

  const handleNotchChange = useCallback((n: number) => {
    if (limited) return; // slider locked in limited mode
    setNotch(n);
    postPreset(n, false);
  }, [limited, postPreset]);

  const handleLimitedToggle = useCallback(() => {
    const next = !limited;
    setLimited(next);
    postPreset(notch, next);
  }, [limited, notch, postPreset]);

  return (
    <div className="h-card">
      <div className="h-card-header" style={{ cursor: 'default', flexDirection: 'column', alignItems: 'flex-start', gap: 8 }}>
        {/* Limited mode toggle */}
        <div style={{ display: 'flex', alignItems: 'center', gap: 10, width: '100%' }}>
          <span style={{ fontWeight: 600, fontSize: 'var(--text-md)' }}>Inference Routing</span>
          <div style={{ marginLeft: 'auto', display: 'flex', alignItems: 'center', gap: 6 }}>
            <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-secondary)' }}>Limited (GPU only)</span>
            <button
              onClick={handleLimitedToggle}
              disabled={saving}
              style={{
                width: 36, height: 20, borderRadius: 10, border: 'none', cursor: 'pointer',
                background: limited ? 'var(--h-amber)' : 'var(--h-bg-hover)',
                position: 'relative', transition: 'background 0.2s',
              }}
              title={limited ? 'Limited mode active — GPU only' : 'Enable limited mode (GPU only)'}
            >
              <span style={{
                position: 'absolute', top: 2, width: 16, height: 16, borderRadius: '50%',
                background: 'white', transition: 'left 0.2s',
                left: limited ? 18 : 2,
              }} />
            </button>
          </div>
        </div>

        {/* Active preset summary */}
        {limited ? (
          <div style={{ fontSize: 'var(--text-sm)', color: 'var(--status-warning)' }}>
            <strong>LIMITED</strong> — GPU only · 1 worker · $0/day
          </div>
        ) : (
          <div style={{ fontSize: 'var(--text-sm)', color: 'var(--text-secondary)' }}>
            <span style={{ color: 'var(--h-teal)', fontWeight: 600 }}>{preset.name}</span>
            {' · '}{preset.workers} workers · {preset.costPerDay}
          </div>
        )}
      </div>

      <div className="h-card-body">
        {/* Notch track — SPOL-09: refined with token sizing/transitions */}
        <div style={{ position: 'relative', padding: 'var(--space-2) 0 var(--space-1)' }}>
          <div style={{
            position: 'relative', height: 4, borderRadius: 'var(--radius-sm)',
            background: 'var(--border-subtle)', marginBottom: 'var(--space-6)',
            opacity: limited ? 0.35 : 1,
          }}>
            {/* Track fill: local (green) → cloud (info blue) gradient */}
            <div style={{
              position: 'absolute', left: 0, top: 0, height: '100%',
              borderRadius: 'var(--radius-sm)',
              width: `${((notch - 1) / 9) * 100}%`,
              background: 'linear-gradient(90deg, var(--status-success), var(--status-info))',
              transition: `width var(--transition-default)`,
            }} />

            {/* Notch dots */}
            {PRESETS.map((p) => {
              const pct = ((p.notch - 1) / 9) * 100;
              const isActive = p.notch === notch && !limited;
              return (
                <div
                  key={p.notch}
                  onClick={() => handleNotchChange(p.notch)}
                  style={{
                    position: 'absolute',
                    left: `${pct}%`,
                    top: '50%',
                    transform: 'translate(-50%, -50%)',
                    width: isActive ? 12 : 8,
                    height: isActive ? 12 : 8,
                    borderRadius: '50%',
                    background: isActive ? 'var(--accent-primary)' : 'var(--border-default)',
                    border: `2px solid ${isActive ? 'var(--accent-primary)' : 'var(--bg-surface)'}`,
                    cursor: limited ? 'default' : 'pointer',
                    transition: `all var(--transition-fast)`,
                    zIndex: 1,
                    boxShadow: isActive ? '0 0 0 3px var(--accent-primary-subtle)' : 'none',
                  }}
                  title={`${p.name} · ${p.description} · ${p.costPerDay}`}
                />
              );
            })}
          </div>

          {/* Endpoint labels */}
          <div style={{
            display: 'flex', justifyContent: 'space-between',
            fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)', marginTop: 'var(--space-1)',
          }}>
            <span>Local baseline · $0/day</span>
            <span>Full swarm · $30+/day</span>
          </div>
        </div>

        {/* Selected preset detail card */}
        {!limited && (
          <div style={{
            marginTop: 'var(--space-2)',
            padding: 'var(--space-2) var(--space-3)',
            borderRadius: 'var(--radius-md)',
            background: 'var(--bg-surface-raised)',
            border: '1px solid var(--border-subtle)',
          }}>
            <div style={{ color: 'var(--accent-primary)', fontWeight: 600, fontSize: 'var(--text-sm)', marginBottom: 2 }}>
              {preset.notch}/10 — {preset.name}
            </div>
            <div style={{ color: 'var(--text-secondary)', fontSize: 'var(--text-xs)' }}>{preset.description}</div>
          </div>
        )}

        {/* CCTX-07 / SPOL-09: Compression — segmented control */}
        <div style={{ marginTop: 'var(--space-3)', paddingTop: 'var(--space-3)', borderTop: '1px solid var(--border-subtle)' }}>
          <div style={{
            fontSize: 'var(--text-xs)',
            color: 'var(--text-tertiary)',
            textTransform: 'uppercase',
            letterSpacing: '0.08em',
            marginBottom: 'var(--space-2)',
            fontWeight: 600,
          }}>
            Context Compression
          </div>
          <div style={{ display: 'flex', gap: 'var(--space-1)', borderRadius: 'var(--radius-md)', overflow: 'hidden', border: '1px solid var(--border-subtle)' }}>
            {(['off', 'light', 'moderate'] as CompressionLevel[]).map(level => (
              <button
                key={level}
                onClick={() => {
                  setCompression(level);
                  getAggregationClient().request('harmony', '/commands/compression-level', {
                    method: 'POST',
                    body: JSON.stringify({ level }),
                  }).catch(() => {});
                }}
                style={{
                  flex: 1,
                  padding: 'var(--space-1) 0',
                  fontSize: 'var(--text-xs)',
                  fontWeight: compression === level ? 500 : 400,
                  border: 'none',
                  borderRight: level !== 'moderate' ? '1px solid var(--border-subtle)' : 'none',
                  borderRadius: 0,
                  background: compression === level ? 'var(--accent-primary-subtle)' : 'transparent',
                  color: compression === level ? 'var(--accent-primary)' : 'var(--text-secondary)',
                  cursor: 'pointer',
                  transition: `background var(--transition-fast), color var(--transition-fast)`,
                  textTransform: 'capitalize',
                }}
              >
                {level}
              </button>
            ))}
          </div>
        </div>

        {saving && (
          <div style={{ fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)', marginTop: 'var(--space-1)' }}>saving…</div>
        )}
        {saveError && (
          <div style={{ fontSize: 'var(--text-xs)', color: 'var(--status-error)', marginTop: 'var(--space-1)' }}>{saveError}</div>
        )}
      </div>
    </div>
  );
}
