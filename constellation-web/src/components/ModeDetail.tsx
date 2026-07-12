// MODE-05: Routing detail panel for the active operating mode.
// Reads from /api/mode which returns execution, review, and triage routing
// derived from ModeConfig — no routing tables duplicated here.

interface WeightedProvider {
  provider: string;
  weight: number;
  model: string;
}

interface TierRoute {
  tier: string;
  providers: WeightedProvider[];
}

interface ProviderModel {
  provider: string;
  model: string;
}

interface ReviewRouting {
  primary: ProviderModel;
  secondary: ProviderModel[];
}

interface TriageStep {
  step: number;
  provider: string;
  model: string;
  context_level: number;
  chord_swap: string | null;
}

export interface ModeRouting {
  execution: TierRoute[];
  review: ReviewRouting;
  triage: TriageStep[];
  max_workers: number;
  daily_budget_target: number;
  description: string;
}

interface Props {
  routing: ModeRouting;
}

const PROVIDER_COLORS: Record<string, string> = {
  local_gpu: 'var(--accent-primary)',
  local_cpu: 'var(--accent-secondary, #4ec9b0)',
  claude:    '#d4a574',
  codex:     '#74b0d4',
  gemini:    '#74d4a0',
};

function providerColor(name: string): string {
  return PROVIDER_COLORS[name] ?? 'var(--text-secondary)';
}

function ProviderPill({ provider, model, weight }: { provider: string; model: string; weight?: number }) {
  return (
    <span style={{
      display: 'inline-flex',
      alignItems: 'center',
      gap: 4,
      padding: '2px 7px',
      borderRadius: 'var(--radius-full, 999px)',
      background: 'var(--bg-surface-raised)',
      border: `1px solid ${providerColor(provider)}44`,
      fontSize: 'var(--text-xs)',
      fontFamily: 'var(--font-mono)',
      color: providerColor(provider),
      whiteSpace: 'nowrap',
    }}>
      {provider}
      {weight !== undefined && weight < 100 && (
        <span style={{ color: 'var(--text-tertiary)', fontSize: '0.85em' }}>×{weight}</span>
      )}
      <span style={{ color: 'var(--text-tertiary)', fontSize: '0.85em' }}>{model}</span>
    </span>
  );
}

function SectionLabel({ children }: { children: React.ReactNode }) {
  return (
    <div style={{
      fontSize: 'var(--text-xs)',
      fontWeight: 600,
      color: 'var(--text-tertiary)',
      textTransform: 'uppercase',
      letterSpacing: '0.06em',
      marginBottom: 5,
      marginTop: 10,
    }}>
      {children}
    </div>
  );
}

export function ModeDetail({ routing }: Props) {
  return (
    <div style={{ padding: '4px 0 2px', display: 'flex', flexDirection: 'column', gap: 0 }}>

      {/* Execution routing */}
      <SectionLabel>Execution</SectionLabel>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 5 }}>
        {routing.execution.map(route => (
          <div key={route.tier} style={{ display: 'flex', alignItems: 'flex-start', gap: 8 }}>
            <span style={{
              fontSize: 'var(--text-xs)',
              color: 'var(--text-tertiary)',
              fontFamily: 'var(--font-mono)',
              minWidth: 54,
              paddingTop: 2,
              flexShrink: 0,
            }}>
              {route.tier}
            </span>
            <div style={{ display: 'flex', flexWrap: 'wrap', gap: 4 }}>
              {route.providers.map((wp, i) => (
                <ProviderPill
                  key={i}
                  provider={wp.provider}
                  model={wp.model}
                  weight={route.providers.length > 1 ? wp.weight : undefined}
                />
              ))}
            </div>
          </div>
        ))}
      </div>

      {/* Review routing */}
      <SectionLabel>Review</SectionLabel>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
        <div style={{ display: 'flex', alignItems: 'flex-start', gap: 8 }}>
          <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)', minWidth: 54, paddingTop: 2, flexShrink: 0 }}>
            primary
          </span>
          <ProviderPill provider={routing.review.primary.provider} model={routing.review.primary.model} />
        </div>
        {routing.review.secondary.length > 0 && (
          <div style={{ display: 'flex', alignItems: 'flex-start', gap: 8 }}>
            <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)', minWidth: 54, paddingTop: 2, flexShrink: 0 }}>
              {routing.review.secondary.length === 1 ? 'dual' : 'triple'}
            </span>
            <div style={{ display: 'flex', flexWrap: 'wrap', gap: 4 }}>
              {routing.review.secondary.map((pm, i) => (
                <ProviderPill key={i} provider={pm.provider} model={pm.model} />
              ))}
            </div>
          </div>
        )}
      </div>

      {/* Triage routing */}
      <SectionLabel>Triage</SectionLabel>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 3 }}>
        {routing.triage.map(step => (
          <div key={step.step} style={{ display: 'flex', alignItems: 'flex-start', gap: 8 }}>
            <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)', minWidth: 54, paddingTop: 2, flexShrink: 0, fontFamily: 'var(--font-mono)' }}>
              step {step.step}
            </span>
            <div style={{ display: 'flex', alignItems: 'center', flexWrap: 'wrap', gap: 4 }}>
              {step.provider ? (
                <ProviderPill provider={step.provider} model={step.model} />
              ) : (
                <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)', fontStyle: 'italic' }}>retry same</span>
              )}
              <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)', fontFamily: 'var(--font-mono)' }}>
                ctx:{step.context_level}
              </span>
              {step.chord_swap && (
                <span style={{ fontSize: 'var(--text-xs)', color: 'var(--accent-warning, #e2a84b)', fontFamily: 'var(--font-mono)' }}>
                  ⇄{step.chord_swap}
                </span>
              )}
            </div>
          </div>
        ))}
      </div>

      {/* Budget target */}
      {routing.daily_budget_target > 0 && (
        <div style={{
          marginTop: 10,
          padding: '4px 8px',
          borderRadius: 'var(--radius-sm)',
          background: 'var(--bg-surface-raised)',
          border: '1px solid var(--border-default)',
          display: 'flex',
          justifyContent: 'space-between',
          alignItems: 'center',
        }}>
          <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)' }}>budget target</span>
          <span style={{ fontSize: 'var(--text-xs)', fontFamily: 'var(--font-mono)', color: 'var(--accent-primary)' }}>
            ${routing.daily_budget_target}/day
          </span>
        </div>
      )}
    </div>
  );
}
