// TRIAGE-06: Horizontal stepper showing triage escalation progress.
import type { TriageStepOutcome } from '../types/engine';
import { ESCALATION_STEPS } from '../types/engine';

interface EscalationStepperProps {
  currentStep: number;           // 1-5
  stepOutcomes: TriageStepOutcome[];
}

export function EscalationStepper({ currentStep, stepOutcomes }: EscalationStepperProps) {
  const outcomeByStep: Record<number, TriageStepOutcome> = {};
  for (const o of stepOutcomes) {
    const idx = ESCALATION_STEPS.findIndex(s => s.name === o.step || String(s.step) === o.step);
    if (idx >= 0) outcomeByStep[idx + 1] = o;
  }

  return (
    <div style={{
      display: 'flex',
      alignItems: 'center',
      gap: 4,
      padding: '8px 12px',
      overflowX: 'auto',
    }}>
      {ESCALATION_STEPS.map(({ step, short }, idx) => {
        const outcome = outcomeByStep[step];
        const isCurrent = step === currentStep;
        const isPast = step < currentStep;
        const isFuture = step > currentStep;

        let icon = '○';
        let color = 'var(--text-tertiary)';
        let bg = 'transparent';
        let fontWeight: number | string = 400;

        if (outcome?.passed) {
          icon = '✓'; color = 'var(--status-success)'; bg = 'rgba(34,197,94,0.1)';
        } else if (isPast && outcome && !outcome.passed) {
          icon = '✗'; color = 'var(--status-error)'; bg = 'rgba(239,68,68,0.1)';
        } else if (isCurrent) {
          icon = '↻'; color = 'var(--status-warning)'; fontWeight = 700;
          bg = 'rgba(245,158,11,0.15)';
        } else if (isFuture) {
          icon = '○'; color = 'var(--text-tertiary)';
        }

        return (
          <div key={step} style={{ display: 'flex', alignItems: 'center', gap: 4 }}>
            <div
              title={`Step ${step}: ${ESCALATION_STEPS[idx].name}`}
              className={isCurrent ? 'triage-pulse' : undefined}
              style={{
                display: 'flex',
                alignItems: 'center',
                gap: 4,
                padding: '2px 8px',
                borderRadius: 12,
                background: bg,
                fontSize: 11,
                color,
                fontWeight,
                whiteSpace: 'nowrap',
              }}>
              <span>{icon}</span>
              <span>{short}</span>
            </div>
            {step < 5 && (
              <span style={{ color: 'var(--border-subtle)', fontSize: 10 }}>→</span>
            )}
          </div>
        );
      })}
    </div>
  );
}
