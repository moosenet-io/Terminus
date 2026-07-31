// MGUI-11/12/13 (S129): the three Settings screens from the guide — module control
// (12), integrations & connections (13), and acquisition & safety (14) — as one
// panel with three sections, because the guide presents them as one settings surface
// reached by sub-navigation rather than three unrelated destinations.
//
// EVERYTHING HERE IS DISPLAY-ONLY, and that is a deliberate safety decision, not an
// unfinished feature:
//
//   - `/api/settings` HAS a PUT. Exposing writes from a read-only browse surface is a
//     separate, operator-gated change with its own review.
//   - The acquisition gates in particular are the DUAL SAFETY GATE for a path with
//     real-world blast radius (it grabs files). The guide itself captions that screen
//     "Write-path · default OFF". A toggle here would put a live-grab switch one
//     stray click away inside a panel whose entire contract is "look, don't touch".
//
// So gates and toggles render as STATE with their current value and their env-var
// provenance. The panel says so out loud rather than leaving a dead switch that looks
// broken.
//
// SECRETS ARE NEVER RENDERED — not even masked. `/api/settings` returns
// `discord_bot_token_masked`, already masked server-side, and this panel still does
// not display it. A mask still leaks shape (length, prefix), and the guide's own
// caption for that screen is "secrets ← <secret-manager> · never authored here". The panel
// shows the VARIABLE NAME and a connected/not-configured state; that is the entire
// useful signal and it leaks nothing.
import { ChartCard } from '../../viz/ChartCard';
import {
  useMuseSettings,
  useMuseIndexers,
  useMuseSubsystems,
  type MuseSettings,
  type MuseSection,
  type MuseSubsystem,
} from '../../hooks/useMuse';

/** The single shared settings read, passed down so the three sections cannot
 *  disagree with one another. */
type SettingsSection = MuseSection<MuseSettings>;

/** A read-only state pill. Deliberately NOT a button: see the module doc. */
function StatePill({ on, onLabel = 'on', offLabel = 'off' }: { on: boolean; onLabel?: string; offLabel?: string }) {
  return (
    <span
      style={{
        padding: '1px 8px',
        fontSize: 'var(--fs-2xs, 10px)',
        fontFamily: 'var(--font-mono)',
        textTransform: 'uppercase',
        letterSpacing: '0.04em',
        borderRadius: 'var(--radius-xs, 3px)',
        color: on ? 'var(--ok, #4ade80)' : 'var(--text-400, var(--text-300))',
        border: `1px solid ${on ? 'var(--ok, #4ade80)' : 'var(--border)'}`,
        whiteSpace: 'nowrap',
      }}
    >
      {on ? onLabel : offLabel}
    </span>
  );
}

/** Distinct from on/off: a value this surface genuinely cannot see. Rendering it as
 *  `off` would be an invented fact, and on a safety gate that is the dangerous
 *  direction. */
function UnknownPill() {
  return (
    <span
      style={{
        padding: '1px 8px',
        fontSize: 'var(--fs-2xs, 10px)',
        fontFamily: 'var(--font-mono)',
        textTransform: 'uppercase',
        letterSpacing: '0.04em',
        borderRadius: 'var(--radius-xs, 3px)',
        color: 'var(--text-400, var(--text-300))',
        border: '1px dashed var(--border)',
        whiteSpace: 'nowrap',
      }}
    >
      unknown
    </span>
  );
}

function SettingRow({
  label,
  detail,
  right,
}: {
  label: string;
  detail?: string;
  right: React.ReactNode;
}) {
  return (
    <div
      style={{
        display: 'grid',
        gridTemplateColumns: '1fr auto',
        gap: 'var(--space-2)',
        alignItems: 'center',
        padding: '5px 0',
        borderBottom: '1px solid var(--border-subtle, rgba(255,255,255,0.05))',
      }}
    >
      <div style={{ minWidth: 0 }}>
        <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-100)' }}>{label}</div>
        {detail && (
          <div style={{ fontSize: 'var(--fs-2xs, 10px)', color: 'var(--text-300)', fontFamily: 'var(--font-mono)' }}>
            {detail}
          </div>
        )}
      </div>
      <div>{right}</div>
    </div>
  );
}

function SectionNote({ children }: { children: React.ReactNode }) {
  return (
    <div
      style={{
        fontSize: 'var(--fs-2xs, 10px)',
        color: 'var(--text-400, var(--text-300))',
        marginBottom: 'var(--space-2)',
        lineHeight: 1.5,
      }}
    >
      {children}
    </div>
  );
}

/** MGUI-13 — guide screen 14. The dual safety gate, shown as state.
 *
 * THE GATE DEFINITION IS THE GUIDE'S, NOT A GUESS — and my first version got it
 * wrong in a way that could have MISREPRESENTED SAFETY, which both reviewers caught:
 *
 *   guide GATE 1 · master : ExperienceSettings.acquisition.enabled
 *   guide GATE 2 · tier   : MUSE_ARR_REQUEST_AUTO_TIER_ENABLED
 *
 * I had used the top-level `master_enabled` (a DIFFERENT setting — the module master
 * switch) as gate 1, and never read the tier gate at all. That could have reported
 * "safe" while the real auto-tier gate was armed.
 *
 * `/api/settings` exposes `acquisition.enabled` but NO tier key (verified against the
 * live payload). So gate 2 is genuinely unknowable from this endpoint, and the panel
 * says so instead of substituting a field that happens to be nearby.
 *
 * The RESULT is therefore only stated when it is actually determinable:
 *   gate 1 OFF  -> SAFE. Sound regardless of gate 2: either gate off means the
 *                  request is persisted for review and never actioned.
 *   gate 1 ON   -> INDETERMINATE from here. It would be armed only if gate 2 is also
 *                  on, and this surface cannot see gate 2. Claiming "armed" or "safe"
 *                  would both be guesses, and on a live-grab switch a wrong guess in
 *                  either direction is the dangerous kind.
 */
function AcquisitionSettings({ settings }: { settings: SettingsSection }) {
  const { data, loading, degraded } = settings;

  // SETTLED-ONLY. `useMuseSection` clears `data` on degrade and on error, and
  // `ChartCard` renders a skeleton instead of children while `loading` — so today a
  // stale gate value cannot reach the screen. This guard makes that property LOCAL
  // anyway: a safety readout must not depend on a sibling component's rendering
  // choice or on a hook's clearing behaviour staying as it is. `fetchOnce` sets
  // `loading` before every refresh while `data` still holds the PREVIOUS snapshot, so
  // without this the gate would be computed from a value that is being re-read
  // (reviewers' final point, and correct in principle even though ChartCard currently
  // masks it).
  const settled = !loading && !degraded && data !== null;
  // Gate 1 is Muse's OWN effective predicate, verified in source:
  //
  //   settings/mod.rs:143  fn is_acquisition_enabled(&self) -> bool {
  //                            self.master_enabled && self.acquisition.enabled
  //                        }
  //
  // The guide labels gate 1 as `ExperienceSettings.acquisition.enabled`, and my first
  // version used that alone. It is not wrong so much as INCOMPLETE: Muse additionally
  // requires master_enabled, so `acquisition.enabled === true` with master off would
  // have shown "indeterminate" for a state that is in fact safe. The MGUI-08 agent
  // building the sibling lifecycle panel caught the divergence.
  //
  // Mirroring the backend predicate rather than the guide's label is the right call
  // for a safety readout: what matters is what the SERVER will actually do, not how
  // the mockup named the field. The row's detail line names both inputs so the
  // provenance stays legible.
  const gate1 = settled ? data.master_enabled && data.acquisition.enabled : null;
  // Gate 2 (MUSE_ARR_REQUEST_AUTO_TIER_ENABLED) is an env var this endpoint does not
  // return. Represented as unknown, never inferred.
  const gate2Known = false;

  const result: { label: string; on: boolean } | null =
    gate1 === false ? { label: 'safe', on: false } : null;

  return (
    <ChartCard
      title="Acquisition & safety"
      subtitle="write-path · display only"
      height={250}
      loading={loading}
      degraded={degraded}
    >
      <div style={{ height: '100%', overflowY: 'auto' }}>
        <SectionNote>
          These gates are shown as <strong>state, not controls</strong>. Both must be on before any
          live grab can fire; either one off means a request is persisted for review and never
          actioned. Changing them is an operator action with real-world blast radius and does not
          belong on a read-only surface.
        </SectionNote>
        <SettingRow
          label="Gate 1 · master"
          detail="master_enabled && acquisition.enabled (Muse is_acquisition_enabled)"
          right={gate1 === null ? <UnknownPill /> : <StatePill on={gate1} />}
        />
        <SettingRow
          label="Gate 2 · tier"
          detail="MUSE_ARR_REQUEST_AUTO_TIER_ENABLED — not exposed by /api/settings"
          right={gate2Known ? <StatePill on={false} /> : <UnknownPill />}
        />
        <SettingRow
          label="Result"
          detail={
            // THREE cases, not two. The previous copy said "gate 1 is on" whenever the
            // result was indeterminate — but that is also true while settings are
            // LOADING or DEGRADED, where gate1 is null and nothing is known. Asserting
            // an unobserved gate state on a safety panel is the same defect as the
            // original gate bug, just quieter (both reviewers caught it).
            gate1 === false
              ? 'gate 1 is off, so a request is persisted for review and never actioned — this holds whatever gate 2 is'
              : gate1 === true
                ? 'cannot be determined here: gate 1 is on and gate 2 is not visible to this surface'
                : 'gate state not available — settings could not be read, so nothing is claimed either way'
          }
          right={result ? <StatePill on={result.on} onLabel="grab armed" offLabel="safe" /> : <UnknownPill />}
        />
      </div>
    </ChartCard>
  );
}

/** MGUI-12 — guide screen 13. Connections + env-var provenance, never a value. */
function IntegrationsSettings({ settings }: { settings: SettingsSection }) {
  const { data, loading, degraded } = useMuseIndexers();
  const indexers = data?.indexers ?? [];

  return (
    <ChartCard
      title="Integrations & connections"
      subtitle="secrets ← <secret-manager> · never shown here"
      height={280}
      loading={loading}
      degraded={degraded}
    >
      <div style={{ height: '100%', overflowY: 'auto' }}>
        <SectionNote>
          Variable names and connection state only. No secret value is rendered — not even a masked
          one, since a mask still leaks its length and prefix.
        </SectionNote>

        {/* THREE states, not two. `configured && reachable` collapsed
            configured-but-unreachable into "not configured", hiding a FAULT behind
            what reads as an un-done setup step — they need different responses
            (reviewer finding). */}
        <SettingRow
          label="Prowlarr (indexers)"
          detail={
            data
              ? `configured=${data.configured} · reachable=${data.reachable} · ${indexers.length} indexer${indexers.length === 1 ? '' : 's'}`
              : undefined
          }
          right={
            !data ? (
              <UnknownPill />
            ) : !data.configured ? (
              <StatePill on={false} offLabel="not configured" />
            ) : data.reachable ? (
              <StatePill on onLabel="connected" />
            ) : (
              <span
                style={{
                  padding: '1px 8px',
                  fontSize: 'var(--fs-2xs, 10px)',
                  fontFamily: 'var(--font-mono)',
                  textTransform: 'uppercase',
                  letterSpacing: '0.04em',
                  borderRadius: 'var(--radius-xs, 3px)',
                  color: 'var(--warn, #fbbf24)',
                  border: '1px solid var(--warn, #fbbf24)',
                  whiteSpace: 'nowrap',
                }}
              >
                unreachable
              </span>
            )
          }
        />

        {indexers.map(ix => (
          <SettingRow
            key={ix.id}
            label={ix.name}
            detail={`${ix.protocol} · ${ix.privacy} · ${ix.categories.length} categories`}
            right={<StatePill on={ix.enabled} onLabel="enabled" offLabel="disabled" />}
          />
        ))}

        {/* Absent from the payload means NOT CONFIGURED, which is different from
            "disconnected" — one is a setup step, the other is a fault. The token is
            never rendered, masked or otherwise; only whether one exists. */}
        <SettingRow
          label="Discord bot"
          detail="DISCORD_BOT_TOKEN (value never displayed)"
          right={
            // Absent settings data is UNKNOWN, not "not configured" — otherwise a
            // still-loading or degraded settings call reads as a definite negative
            // (reviewer finding).
            !settings.data ? (
              <UnknownPill />
            ) : (
              <StatePill
                on={settings.data.discord_bot.enabled}
                onLabel="enabled"
                offLabel={settings.data.discord_bot_token_masked ? 'configured, off' : 'not configured'}
              />
            )
          }
        />
      </div>
    </ChartCard>
  );
}

/** MGUI-11 — guide screen 12. Module registry + per-subsystem wiring. */
function ModuleSettings({ settings }: { settings: SettingsSection }) {
  const { data, loading, degraded } = settings;
  const subs = useMuseSubsystems();

  const modules: { label: string; detail: string; on: boolean }[] = data
    ? [
        { label: 'Channel director', detail: `serendipity ${data.channel_director.serendipity_percent}%`, on: data.channel_director.enabled },
        { label: 'Adaptation loop', detail: `aggressiveness ${data.adaptation_loop.aggressiveness}`, on: data.adaptation_loop.enabled },
        {
          label: 'KG visualisations',
          detail: `neighbour threshold ${data.kg_viz.taste_neighbor_threshold} · watch-history limit ${data.kg_viz.watch_history_limit}`,
          on: data.kg_viz.enabled,
        },
        { label: 'Watch together', detail: '', on: data.watch_together.enabled },
        {
          label: "What's hot",
          detail: `${Object.keys(data.whats_hot.source_weights).length} weighted source${Object.keys(data.whats_hot.source_weights).length === 1 ? '' : 's'}`,
          on: data.whats_hot.enabled,
        },
        {
          label: 'Discord bot',
          detail: `cadence ${data.discord_bot.promotion_cadence_secs}s · match ≥ ${data.discord_bot.promotion_match_threshold} · ${data.discord_bot.trusted_friends.length} trusted friend${data.discord_bot.trusted_friends.length === 1 ? '' : 's'}`,
          on: data.discord_bot.enabled,
        },
      ]
    : [];

  return (
    <ChartCard
      title="Module control"
      subtitle={
        data
          ? `sharing: ${data.sharing.granularity} · questions: ${data.question_frequency.frequency}${data.question_frequency.silent_mode ? ' (silent)' : ''} · ${data.personas.length} persona${data.personas.length === 1 ? '' : 's'}`
          : 'module registry'
      }
      height={300}
      loading={loading}
      degraded={degraded}
    >
      <div style={{ height: '100%', overflowY: 'auto' }}>
        <SectionNote>
          Enable states are shown as <strong>state, not toggles</strong> — `/api/settings` has a PUT,
          but exposing writes from this surface is a separate operator-gated change.
        </SectionNote>
        {modules.map(m => (
          <SettingRow key={m.label} label={m.label} detail={m.detail || undefined} right={<StatePill on={m.on} />} />
        ))}
        {/* The subsystem wiring the guide pairs with this screen. Reuses the same
            source as the dashboard grid rather than a second notion of "wired". */}
        {(subs.data?.subsystems ?? []).map((s: MuseSubsystem) => (
          <SettingRow key={s.key} label={s.label} detail={s.concern} right={
            <span style={{ fontSize: 'var(--fs-2xs, 10px)', fontFamily: 'var(--font-mono)', color: 'var(--text-300)' }}>{s.state}</span>
          } />
        ))}
      </div>
    </ChartCard>
  );
}

export function SettingsPanel() {
  // ONE settings fetch, shared. Three independent `useMuseSettings()` calls meant
  // three requests AND three snapshots that could disagree with each other mid-flight
  // — a settings page showing two different values for the same underlying config is
  // worse than a slow one (reviewer finding).
  const settings = useMuseSettings();
  return (
    <div style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <AcquisitionSettings settings={settings} />
      <IntegrationsSettings settings={settings} />
      <ModuleSettings settings={settings} />
    </div>
  );
}
