// ACARD-02: Single agent lane — vertical card showing one agent's state
import { useEffect, useRef, useState } from 'react';
import type { AgentActivity, AgentStep } from '../types/api';

// Inline keyframes injected once per document
const SPIN_STYLE_ID = 'acard-spin-keyframes';
if (typeof document !== 'undefined' && !document.getElementById(SPIN_STYLE_ID)) {
  const style = document.createElement('style');
  style.id = SPIN_STYLE_ID;
  style.textContent = `@keyframes acard-spin { to { transform: rotate(360deg); } }`;
  document.head.appendChild(style);
}

interface Props {
  agent: AgentActivity;
  /** TRIAGE-06: true when this lane's task is the currently-triaging task */
  inTriageMode?: boolean;
}

function formatElapsed(secs: number): string {
  if (secs <= 0) return '';
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  if (m > 0) return `${m}m${String(s).padStart(2, '0')}s`;
  return `${s}s`;
}

function providerIcon(p: string): string {
  const lower = p.toLowerCase();
  if (lower.includes('ollama') || lower.includes('local')) return '🖥';
  if (lower.includes('gitea') || lower.includes('git')) return '⎇';
  if (lower.includes('plane')) return '☰';
  return '☁';
}

/** Map provider codename → infrastructure type icon.
 *  Cloud providers → ☁  llama-server → 🗄  Ollama/local → 💾  unknown → ? */
function providerTypeIcon(provider?: string): { icon: string; label: string } | null {
  if (!provider) return null;
  const p = provider.toLowerCase();
  if (p === 'llama') return { icon: '🗄', label: 'llama-server (GPU)' };
  if (p === 'local') return { icon: '💾', label: 'Ollama (local)' };
  if (p === 'claude' || p === 'codex' || p === 'gemini')
    return { icon: '☁', label: `${provider} (cloud)` };
  return { icon: '?', label: provider };
}

/** Colored tier badge: quick=green, standard=blue, deep=purple. */
function TierBadge({ tier }: { tier?: string }) {
  if (!tier || tier === '—') return null;
  const cfg: Record<string, { bg: string; color: string }> = {
    quick:    { bg: 'rgba(34,197,94,0.15)',  color: '#22c55e' },
    standard: { bg: 'rgba(59,130,246,0.15)', color: '#3b82f6' },
    deep:     { bg: 'rgba(168,85,247,0.15)', color: '#a855f7' },
  };
  const style = cfg[tier.toLowerCase()] ?? { bg: 'var(--bg-surface-raised)', color: 'var(--text-tertiary)' };
  return (
    <span style={{
      fontSize: 10,
      padding: '1px 6px',
      borderRadius: 3,
      background: style.bg,
      color: style.color,
      fontWeight: 600,
      textTransform: 'capitalize',
      flexShrink: 0,
    }}>
      {tier}
    </span>
  );
}

function StepIcon({ state }: { state: AgentStep['state'] }) {
  if (state === 'done') {
    return (
      <span style={{ color: 'var(--status-success)', fontSize: 12, display: 'inline-block', width: 14, textAlign: 'center' }}>
        ✓
      </span>
    );
  }
  if (state === 'active') {
    return (
      <span style={{
        color: 'var(--status-info)',
        fontSize: 13,
        display: 'inline-block',
        width: 14,
        textAlign: 'center',
        animation: 'acard-spin 1s linear infinite',
        transformOrigin: 'center',
      }}>
        ↻
      </span>
    );
  }
  if (state === 'failed') {
    return (
      <span style={{ color: 'var(--status-error)', fontSize: 12, display: 'inline-block', width: 14, textAlign: 'center' }}>
        ✕
      </span>
    );
  }
  // pending
  return (
    <span style={{ color: 'var(--text-tertiary)', fontSize: 12, display: 'inline-block', width: 14, textAlign: 'center' }}>
      ○
    </span>
  );
}

export function AgentLane({ agent, inTriageMode = false }: Props) {
  const isActive = agent.status === 'active';

  // Live elapsed timer
  const [elapsed, setElapsed] = useState(agent.elapsed_seconds);
  const startRef = useRef(Date.now() - agent.elapsed_seconds * 1000);

  useEffect(() => {
    if (!isActive) {
      setElapsed(agent.elapsed_seconds);
      return;
    }
    startRef.current = Date.now() - agent.elapsed_seconds * 1000;
    const id = setInterval(() => {
      setElapsed(Math.floor((Date.now() - startRef.current) / 1000));
    }, 1000);
    return () => clearInterval(id);
  }, [agent.elapsed_seconds, isActive]);

  const dotColor = isActive
    ? 'var(--status-success)'
    : agent.status === 'cooldown'
      ? 'var(--status-warning)'
      : 'var(--text-tertiary)';

  const elapsedStr = formatElapsed(elapsed);

  // TRIAGE-06: amber border + badge when this lane is in triage mode
  const triageBorder = inTriageMode ? '2px solid var(--status-warning)' : undefined;

  return (
    <div
      className={`h-card${inTriageMode ? ' triage-pulse' : ''}`}
      style={{
        minWidth: 200,
        flex: '1 1 0',
        display: 'flex',
        flexDirection: 'column',
        gap: 0,
        opacity: isActive ? 1 : 0.7,
        border: triageBorder,
      }}
    >
      {/* Header row */}
      <div style={{
        display: 'flex',
        alignItems: 'center',
        gap: 6,
        padding: '10px 12px 4px',
        flexWrap: 'wrap',
      }}>
        <span style={{
          width: 8,
          height: 8,
          borderRadius: '50%',
          background: dotColor,
          flexShrink: 0,
          boxShadow: isActive ? `0 0 4px ${dotColor}` : 'none',
        }} />
        <span style={{ fontWeight: 600, fontSize: 13, color: 'var(--text-primary)', flex: 1 }}>
          {agent.display_name ?? agent.agent_id}
        </span>
        {inTriageMode ? (
          <span style={{
            fontSize: 10, padding: '1px 6px', borderRadius: 3,
            background: 'rgba(245,158,11,0.2)', color: 'var(--status-warning)',
            fontWeight: 700,
          }}>TRIAGE</span>
        ) : (
          <TierBadge tier={isActive ? agent.tier : undefined} />
        )}
        {elapsedStr && (
          <span style={{ fontSize: 11, color: 'var(--text-tertiary)', fontFamily: 'var(--font-mono)' }}>
            {elapsedStr}
          </span>
        )}
      </div>

      {/* Model */}
      <div style={{ padding: '0 12px 6px', fontSize: 11, color: 'var(--text-tertiary)', fontFamily: 'var(--font-mono)' }}>
        {agent.model}
      </div>

      {/* Task */}
      {agent.task && (
        <div style={{
          padding: '3px 12px 6px',
          fontSize: 11,
          color: 'var(--text-secondary)',
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
        }}>
          <span style={{ color: 'var(--accent-primary)', fontFamily: 'var(--font-mono)', fontSize: 10 }}>
            {agent.task.id}
          </span>
          {' '}
          {agent.task.title}
        </div>
      )}

      {/* Divider */}
      {agent.loop_state && (
        <div style={{ borderTop: '1px solid var(--border-subtle)', margin: '0 12px' }} />
      )}

      {/* Loop steps with pipeline connectors */}
      {agent.loop_state ? (
        <div style={{ padding: '8px 12px', flex: 1 }}>
          {agent.loop_state.steps.map((step, idx) => {
            const isLast = idx === agent.loop_state!.steps.length - 1;
            const nextStep = !isLast ? agent.loop_state!.steps[idx + 1] : null;
            // Connector state: between last-done and active = transitioning, otherwise match the lower step
            const connectorClass =
              step.state === 'done' && nextStep?.state === 'active'
                ? 'step-connector step-connector--pulse'
                : step.state === 'done'
                  ? 'step-connector step-connector--done'
                  : 'step-connector step-connector--pending';

            return (
              <div key={step.name}>
                {/* Step row */}
                <div style={{ display: 'flex', alignItems: 'center', gap: 6, minHeight: 20 }}>
                  <StepIcon state={step.state} />
                  <span style={{
                    fontSize: 11,
                    color: step.state === 'active'
                      ? 'var(--status-info)'
                      : step.state === 'done'
                        ? 'var(--text-secondary)'
                        : 'var(--text-tertiary)',
                    fontWeight: step.state === 'active' ? 600 : 400,
                    textTransform: 'capitalize',
                    width: 52,
                    flexShrink: 0,
                  }}>
                    {step.name}
                  </span>
                  {step.detail && (
                    <span style={{
                      fontSize: 10,
                      color: 'var(--text-tertiary)',
                      overflow: 'hidden',
                      textOverflow: 'ellipsis',
                      whiteSpace: 'nowrap',
                    }}>
                      {step.detail}
                    </span>
                  )}
                </div>
                {/* Vertical connector between steps — 12px tall for visibility */}
                {!isLast && (
                  <div style={{ display: 'flex', alignItems: 'stretch', height: 12 }}>
                    {/* Aligns connector under the step icon dot center */}
                    <div style={{ width: 7, flexShrink: 0 }} />
                    <div className={connectorClass} />
                  </div>
                )}
              </div>
            );
          })}
        </div>
      ) : (
        <div style={{
          flex: 1,
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          padding: '16px 12px',
          color: 'var(--text-tertiary)',
          fontSize: 12,
        }}>
          Waiting for tasks
        </div>
      )}

      {/* Footer: provider-type icon + active tool providers */}
      {(isActive || agent.active_providers.length > 0) && (
        <>
          <div style={{ borderTop: '1px solid var(--border-subtle)', margin: '0 12px' }} />
          <div style={{ padding: '6px 12px 10px', display: 'flex', gap: 6, flexWrap: 'wrap', alignItems: 'center' }}>
            {/* Provider-type icon (cloud/llama/local) — only when a task is active */}
            {isActive && (() => {
              const pt = providerTypeIcon(agent.provider);
              return pt ? (
                <span title={pt.label} style={{ fontSize: 14, flexShrink: 0 }}>{pt.icon}</span>
              ) : null;
            })()}
            {/* Active tool-use providers (plane, gitea, etc.) */}
            {agent.active_providers.map(p => (
              <span key={p} title={p} style={{ fontSize: 14 }}>
                {providerIcon(p)}
              </span>
            ))}
          </div>
        </>
      )}
    </div>
  );
}
