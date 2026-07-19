// LGUI-09 (§3.4): `lumina.persona` panel — Persona & Behavior. Route `/lumina/persona`,
// operator (§2's IA table). Composed sections, each degrading independently per this app's
// convention (see useMuse.ts's doc): trait quartet + radar (one shared state source —
// `useLuminaPersona` + the local `draft*` state below, never a second copy), knowledge digest
// (read-only), active context (editable, gated), layer inspector (read-only, 11 assembler
// layers), and the ceremony card.
import { useEffect, useMemo, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { Card, CardTitle } from '../../components/Card';
import { Badge } from '../../components/Badge';
import { Button } from '../../components/Button';
import { ConfirmDialog } from '../../components/ConfirmDialog';
import { RoleGate } from '../../components/RoleGate';
import { SkeletonList } from '../../components/Skeleton';
import { ChartCard } from '../../viz/ChartCard';
import { RadarChartKit } from '../../viz/RadarChart';
import { TraitSlider } from './TraitSlider';
import {
  useLuminaPersona,
  computeEffective,
  clampToPersonaBounds,
  layerBarWidths,
} from '../../hooks/useLuminaPersona';
import { LUMINA_TRAIT_KEYS, PERSONA_DEFAULT_BOUNDS } from '../../types/lumina';
import type { LuminaTraitKey, LuminaTraitVector } from '../../types/lumina';

type EditKind = 'base' | 'modifier';

interface TraitDiffRow {
  key: LuminaTraitKey;
  from: number;
  to: number;
}

function traitsEqual(a: LuminaTraitVector, b: LuminaTraitVector): boolean {
  return LUMINA_TRAIT_KEYS.every(k => a[k] === b[k]);
}

export function PersonaPanel() {
  const navigate = useNavigate();
  const { persona, loading, isRefetching, error, refetch, status, saveTraits, saveContext } = useLuminaPersona();

  // ── Trait editing draft — the ONE state source the radar + slider quartet both read.
  // `draftBase`/`draftModifier` start `null` (not yet loaded); once `persona` arrives they seed
  // from it and only the operator's own edits (or a save/cancel) change them from then on —
  // an incoming poll refresh never silently clobbers an in-progress edit (see the effect below).
  const [draftBase, setDraftBase] = useState<LuminaTraitVector | null>(null);
  const [draftModifier, setDraftModifier] = useState<LuminaTraitVector | null>(null);
  const [editKind, setEditKind] = useState<EditKind>('base');
  const [targetUser, setTargetUser] = useState('');
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);

  useEffect(() => {
    if (!persona) return;
    setDraftBase(prev => prev ?? persona.traits.base);
    setDraftModifier(prev => prev ?? persona.traits.modifier);
  }, [persona]);

  const bounds = persona?.bounds ?? PERSONA_DEFAULT_BOUNDS;

  const effective = useMemo(() => {
    if (!draftBase || !draftModifier) return null;
    return computeEffective(draftBase, draftModifier, bounds);
  }, [draftBase, draftModifier, bounds]);

  const dirty = !!(persona && draftBase && draftModifier
    && (!traitsEqual(draftBase, persona.traits.base) || !traitsEqual(draftModifier, persona.traits.modifier)));

  const diffRows: TraitDiffRow[] = useMemo(() => {
    if (!persona || !draftBase || !draftModifier) return [];
    const field = editKind === 'base' ? 'base' : 'modifier';
    const draft = editKind === 'base' ? draftBase : draftModifier;
    const current = persona.traits[field];
    return LUMINA_TRAIT_KEYS
      .filter(k => draft[k] !== current[k])
      .map(k => ({ key: k, from: current[k], to: draft[k] }));
  }, [persona, draftBase, draftModifier, editKind]);

  function onTraitChange(key: LuminaTraitKey, next: number) {
    const clamped = clampToPersonaBounds(next, bounds);
    if (editKind === 'base') {
      setDraftBase(prev => (prev ? { ...prev, [key]: clamped } : prev));
    } else {
      setDraftModifier(prev => (prev ? { ...prev, [key]: clamped } : prev));
    }
  }

  function cancelEdits() {
    if (!persona) return;
    setDraftBase(persona.traits.base);
    setDraftModifier(persona.traits.modifier);
  }

  async function confirmSave() {
    if (!draftBase || !draftModifier) return;
    setSaving(true);
    setSaveError(null);
    try {
      await saveTraits({
        base: editKind === 'base' ? draftBase : undefined,
        modifier: editKind === 'modifier' ? draftModifier : undefined,
        user: editKind === 'modifier' && targetUser ? targetUser : undefined,
      });
      setConfirmOpen(false);
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }

  // ── Active context (editable textarea, gated) ────────────────────────────────
  const [contextDraft, setContextDraft] = useState('');
  const [contextSaving, setContextSaving] = useState(false);
  const [contextError, setContextError] = useState<string | null>(null);
  const [contextInitialized, setContextInitialized] = useState(false);

  useEffect(() => {
    if (persona && !contextInitialized) {
      setContextDraft(persona.active_context);
      setContextInitialized(true);
    }
  }, [persona, contextInitialized]);

  async function saveContextDraft() {
    setContextSaving(true);
    setContextError(null);
    try {
      await saveContext(contextDraft);
    } catch (e) {
      setContextError(e instanceof Error ? e.message : String(e));
    } finally {
      setContextSaving(false);
    }
  }

  const radarData = effective
    ? LUMINA_TRAIT_KEYS.map(k => ({
        axis: k,
        value: effective[k],
        deemphasis: draftBase ? draftBase[k] : undefined,
      }))
    : [];

  return (
    <div style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="Trait tuning, knowledge digest, active context, and the assembled prompt layers">
        Lumina — Persona &amp; Behavior
      </CardTitle>

      {error && (
        <Card variant="content">
          <span style={{ color: 'var(--status-error)' }}>{error}</span>
          <div style={{ marginTop: 'var(--space-2)' }}>
            <Button variant="ghost" size="sm" onClick={refetch}>Retry</Button>
          </div>
        </Card>
      )}

      {loading && !error && (
        <Card variant="content"><SkeletonList rows={8} /></Card>
      )}

      {!loading && !error && persona && draftBase && draftModifier && effective && (
        <>
          {status && !status.dynamic_prompt && (
            <Card variant="content" style={{ borderColor: 'var(--status-warning)' }}>
              <Badge tone="amber" glowDot>legacy prompt mode</Badge>
              <div style={{ marginTop: 'var(--space-2)', fontSize: 'var(--fs-sm)', color: 'var(--text-body)' }}>
                <code style={{ fontFamily: 'var(--font-mono)' }}>LUMINA_DYNAMIC_PROMPT=false</code> — this
                instance falls back to the legacy static system prompt; the assembler layers below
                are not actually composed per-turn until it's re-enabled.
              </div>
            </Card>
          )}

          {/* ── Trait quartet + radar ──────────────────────────────────────── */}
          <Card variant="content">
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 'var(--space-3)', flexWrap: 'wrap', gap: 'var(--space-2)' }}>
              <div style={{ display: 'flex', gap: 'var(--space-2)', alignItems: 'center' }}>
                <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)' }}>Editing:</span>
                <RoleGate>
                  <button
                    type="button"
                    className={`h-badge ${editKind === 'base' ? 'h-badge-violet' : 'h-badge-neutral'}`}
                    style={{ cursor: 'pointer', border: 'none' }}
                    onClick={() => setEditKind('base')}
                  >
                    shared base
                  </button>
                </RoleGate>
                <RoleGate>
                  <button
                    type="button"
                    className={`h-badge ${editKind === 'modifier' ? 'h-badge-violet' : 'h-badge-neutral'}`}
                    style={{ cursor: 'pointer', border: 'none' }}
                    onClick={() => setEditKind('modifier')}
                  >
                    per-user modifier (admin-on-behalf)
                  </button>
                </RoleGate>
                {editKind === 'modifier' && (
                  <RoleGate>
                    <input
                      type="text"
                      value={targetUser}
                      onChange={e => setTargetUser(e.target.value)}
                      placeholder="user id (blank = self)"
                      aria-label="Target user for modifier edit"
                      style={{
                        background: 'var(--space-700)', border: '1px solid var(--border)',
                        borderRadius: 'var(--radius-md)', color: 'var(--text-primary)',
                        padding: '4px 8px', fontSize: 'var(--fs-xs)', minWidth: 160,
                      }}
                    />
                  </RoleGate>
                )}
              </div>
              <div style={{ display: 'flex', gap: 'var(--space-2)' }}>
                <RoleGate>
                  <Button variant="ghost" size="sm" onClick={cancelEdits} disabled={!dirty}>
                    Discard
                  </Button>
                </RoleGate>
                <RoleGate>
                  <Button variant="primary" size="sm" onClick={() => setConfirmOpen(true)} disabled={!dirty}>
                    Save changes
                  </Button>
                </RoleGate>
              </div>
            </div>

            <div style={{ display: 'grid', gridTemplateColumns: 'minmax(280px, 1fr) 260px', gap: 'var(--space-4)', alignItems: 'start' }}>
              <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
                {LUMINA_TRAIT_KEYS.map(key => (
                  <RoleGate key={key} display="block">
                    <TraitSlider
                      label={key}
                      base={draftBase[key]}
                      modifier={draftModifier[key]}
                      effective={effective[key]}
                      bounds={bounds}
                      editKind={editKind}
                      editValue={editKind === 'base' ? draftBase[key] : draftModifier[key]}
                      onChange={next => onTraitChange(key, next)}
                    />
                  </RoleGate>
                ))}
              </div>

              <ChartCard title="Trait radar" height={220} isRefetching={isRefetching}>
                <RadarChartKit
                  data={radarData}
                  max={1}
                  height={220}
                  primaryLabel="effective"
                  deemphasisLabel="base"
                />
              </ChartCard>
            </div>
          </Card>

          {/* ── Knowledge digest (read-only) ───────────────────────────────── */}
          <Card variant="content">
            <CardTitle subtitle="What the assistant has learned/inferred about the operator — read-only, feeds the [knowledge] layer">
              Knowledge digest
            </CardTitle>
            <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-body)', lineHeight: 'var(--lh-body)', whiteSpace: 'pre-wrap' }}>
              {persona.knowledge_digest || <span style={{ color: 'var(--text-muted)' }}>No digest yet.</span>}
            </div>
          </Card>

          {/* ── Active context (editable, gated) ───────────────────────────── */}
          <Card variant="content">
            <CardTitle subtitle="Free-text situational context injected as the [context] layer">
              Active context
            </CardTitle>
            <RoleGate display="block">
              <textarea
                value={contextDraft}
                onChange={e => setContextDraft(e.target.value)}
                rows={4}
                style={{
                  width: '100%', resize: 'vertical', background: 'var(--space-700)',
                  border: '1px solid var(--border)', borderRadius: 'var(--radius-md)',
                  color: 'var(--text-primary)', padding: 'var(--space-2)', fontSize: 'var(--fs-sm)',
                  fontFamily: 'var(--font-sans)',
                }}
              />
            </RoleGate>
            <div style={{ marginTop: 'var(--space-2)', display: 'flex', gap: 'var(--space-2)', alignItems: 'center' }}>
              <RoleGate>
                <Button
                  variant="primary"
                  size="sm"
                  onClick={saveContextDraft}
                  disabled={contextSaving || contextDraft === persona.active_context}
                >
                  {contextSaving ? 'Saving…' : 'Save context'}
                </Button>
              </RoleGate>
              {contextError && <span style={{ color: 'var(--status-error)', fontSize: 'var(--fs-xs)' }}>{contextError}</span>}
            </div>
          </Card>

          {/* ── Layer inspector (read-only) ─────────────────────────────────── */}
          <Card variant="content">
            <CardTitle subtitle="The 11 PromptAssembler layers, in composition order — byte size + enabled state">
              Layer inspector
            </CardTitle>
            <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)' }}>
              {layerBarWidths(persona.layers).map(l => (
                <div key={l.name} style={{ display: 'grid', gridTemplateColumns: '110px 1fr 70px 90px', alignItems: 'center', gap: 'var(--space-2)' }}>
                  <span style={{ fontSize: 'var(--fs-xs)', fontFamily: 'var(--font-mono)', color: 'var(--text-body)' }}>[{l.name}]</span>
                  <div style={{ height: 6, borderRadius: 'var(--radius-sm)', background: 'var(--border-subtle)', overflow: 'hidden' }}>
                    <div style={{ height: '100%', width: `${l.pct}%`, background: l.enabled ? 'var(--violet-400)' : 'var(--text-muted)' }} />
                  </div>
                  <span style={{ fontSize: 'var(--fs-xs)', fontFamily: 'var(--font-mono)', color: 'var(--text-muted)', textAlign: 'right' }}>
                    {l.bytes.toLocaleString()} B
                  </span>
                  <Badge tone={l.enabled ? 'green' : 'neutral'}>{l.enabled ? 'enabled' : 'disabled'}</Badge>
                </div>
              ))}
            </div>
          </Card>

          {/* ── Ceremony card ────────────────────────────────────────────────── */}
          <Card variant="content">
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', flexWrap: 'wrap', gap: 'var(--space-2)' }}>
              <div>
                <CardTitle subtitle="First-run naming ceremony status">Ceremony</CardTitle>
                <Badge tone={status?.onboarding_complete ? 'green' : 'amber'} glowDot>
                  {status == null ? 'unknown' : status.onboarding_complete ? 'complete' : 'needs setup'}
                </Badge>
              </div>
              <RoleGate>
                <Button variant="secondary" size="sm" onClick={() => navigate('/lumina/setup')}>
                  Re-run naming ceremony
                </Button>
              </RoleGate>
            </div>
          </Card>
        </>
      )}

      <ConfirmDialog
        open={confirmOpen}
        title={`Save ${editKind === 'base' ? 'shared base' : 'per-user modifier'} traits?`}
        description={
          diffRows.length === 0
            ? 'No changed traits.'
            : diffRows.map(r => `${r.key}: ${r.from.toFixed(2)} → ${r.to.toFixed(2)}`).join('  ·  ')
        }
        confirmLabel="Save"
        busy={saving}
        onConfirm={confirmSave}
        onCancel={() => setConfirmOpen(false)}
      />
      {saveError && (
        <Card variant="content">
          <span style={{ color: 'var(--status-error)' }}>{saveError}</span>
        </Card>
      )}
    </div>
  );
}
