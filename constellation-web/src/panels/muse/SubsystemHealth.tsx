// MGUI-06 (S129): the dashboard's subsystem health grid — guide screen 01's
// "Subsystem health · N modules" block (`src module → concern`, a wiring badge each).
//
// `GET /api/subsystems` is PUBLIC and returns 9 real subsystems today, each with
// `key`, `label`, `concern` and `state`.
//
// The load-bearing rule is the STATE VOCABULARY. The guide defines four wiring
// states, and they mean materially different things to an operator:
//
//   live      — wired and running in this deployment
//   worker    — a background worker drives it on a cadence
//   seam      — implemented + tested, but nothing calls it yet
//   unmounted — declared, not exercised
//
// An unrecognized state renders VERBATIM as "unclassified", never coerced to `live`.
// Defaulting an unknown state to `live` would tell the operator a subsystem is
// running when nobody knows that — the same class of invented-confidence error as a
// fabricated match verdict.
import { useMuseSubsystems, type MuseSubsystem } from '../../hooks/useMuse';
import { ChartCard } from '../../viz/ChartCard';

/** Guide pattern-library tones. `null` tone = unrecognized, rendered neutral. */
function stateStyle(state: string): { label: string; tone: string; known: boolean } {
  switch (state.toLowerCase()) {
    case 'live':
      return { label: 'live', tone: 'var(--ok, #4ade80)', known: true };
    case 'worker':
      return { label: 'worker', tone: 'var(--info, #60a5fa)', known: true };
    case 'seam':
      return { label: 'seam', tone: 'var(--warn, #fbbf24)', known: true };
    case 'unmounted':
      return { label: 'unmounted', tone: 'var(--text-400, var(--text-300))', known: true };
    default:
      // Shown as-is so an operator can see the actual value, flagged as unclassified
      // so it is never mistaken for a known-good state.
      return { label: `${state} (unclassified)`, tone: 'var(--text-300)', known: false };
  }
}

function SubsystemRow({ s }: { s: MuseSubsystem }) {
  const st = stateStyle(s.state);
  return (
    <div
      style={{
        display: 'grid',
        gridTemplateColumns: 'minmax(120px, 180px) 1fr auto',
        gap: 'var(--space-2)',
        alignItems: 'baseline',
        padding: '4px 0',
        borderBottom: '1px solid var(--border-subtle, rgba(255,255,255,0.05))',
      }}
    >
      <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-100)' }} title={s.key}>
        {s.label}
      </div>
      <div style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-300)', lineHeight: 1.4 }}>
        {s.concern}
      </div>
      <div
        style={{
          fontSize: 'var(--fs-2xs, 10px)',
          fontFamily: 'var(--font-mono)',
          color: st.tone,
          whiteSpace: 'nowrap',
          fontStyle: st.known ? 'normal' : 'italic',
        }}
      >
        {st.label}
      </div>
    </div>
  );
}

/** Rendered as its own card inside the dashboard so it degrades ALONE — a failure
 *  here must not take the working stat tiles with it. */
export function SubsystemHealth() {
  const { data, loading, degraded } = useMuseSubsystems();
  const subs = data?.subsystems ?? [];
  const empty = !loading && !degraded && subs.length === 0;

  const liveCount = subs.filter(s => s.state.toLowerCase() === 'live').length;

  return (
    <ChartCard
      title="Subsystem health"
      subtitle={subs.length ? `${liveCount} live of ${subs.length} modules` : 'src module → concern'}
      height={300}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No subsystems reported"
      emptyHint="Muse returned an empty subsystem registry"
    >
      <div style={{ height: '100%', minHeight: 0, overflowY: 'auto' }}>
        {subs.map(s => (
          <SubsystemRow key={s.key} s={s} />
        ))}
      </div>
    </ChartCard>
  );
}
