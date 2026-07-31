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
import { useMuseSettings, useMuseIndexers, useMuseSubsystems, type MuseSubsystem } from '../../hooks/useMuse';

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

/** MGUI-13 — guide screen 14. The dual safety gate, shown as state. */
function AcquisitionSettings() {
  const { data, loading, degraded } = useMuseSettings();
  const master = data?.master_enabled ?? false;
  const acquisition = data?.acquisition.enabled ?? false;
  // BOTH must be on for a live grab. Rendering the conjunction explicitly is the
  // point of the guide's "dual gate" pattern — two separate pills leave the reader to
  // work out the AND, and getting that wrong in either direction is dangerous.
  const canGrab = master && acquisition;

  return (
    <ChartCard
      title="Acquisition & safety"
      subtitle="write-path · display only"
      height={230}
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
        <SettingRow label="Gate 1 · master" detail="ExperienceSettings.master_enabled" right={<StatePill on={master} />} />
        <SettingRow
          label="Gate 2 · acquisition"
          detail="ExperienceSettings.acquisition.enabled"
          right={<StatePill on={acquisition} />}
        />
        <SettingRow
          label="Result"
          detail={canGrab ? 'both gates on — a live grab may fire' : 'a request is persisted for review, never actioned'}
          right={<StatePill on={canGrab} onLabel="grab armed" offLabel="safe" />}
        />
      </div>
    </ChartCard>
  );
}

/** MGUI-12 — guide screen 13. Connections + env-var provenance, never a value. */
function IntegrationsSettings() {
  const { data, loading, degraded } = useMuseIndexers();
  const settings = useMuseSettings();
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

        <SettingRow
          label="Prowlarr (indexers)"
          detail={
            data
              ? `configured=${data.configured} · reachable=${data.reachable} · ${indexers.length} indexer${indexers.length === 1 ? '' : 's'}`
              : undefined
          }
          right={<StatePill on={Boolean(data?.configured && data?.reachable)} onLabel="connected" offLabel="not configured" />}
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
            <StatePill
              on={Boolean(settings.data?.discord_bot.enabled)}
              onLabel="enabled"
              offLabel={settings.data?.discord_bot_token_masked ? 'configured, off' : 'not configured'}
            />
          }
        />
      </div>
    </ChartCard>
  );
}

/** MGUI-11 — guide screen 12. Module registry + per-subsystem wiring. */
function ModuleSettings() {
  const { data, loading, degraded } = useMuseSettings();
  const subs = useMuseSubsystems();

  const modules: { label: string; detail: string; on: boolean }[] = data
    ? [
        { label: 'Channel director', detail: `serendipity ${data.channel_director.serendipity_percent}%`, on: data.channel_director.enabled },
        { label: 'Adaptation loop', detail: `aggressiveness ${data.adaptation_loop.aggressiveness}`, on: data.adaptation_loop.enabled },
        { label: 'KG visualisations', detail: `watch-history limit ${data.kg_viz.watch_history_limit}`, on: data.kg_viz.enabled },
        { label: 'Watch together', detail: '', on: data.watch_together.enabled },
        { label: "What's hot", detail: '', on: data.whats_hot.enabled },
        { label: 'Discord bot', detail: `cadence ${data.discord_bot.promotion_cadence_secs}s`, on: data.discord_bot.enabled },
      ]
    : [];

  return (
    <ChartCard
      title="Module control"
      subtitle={data ? `sharing: ${data.sharing.granularity} · questions: ${data.question_frequency.frequency}` : 'module registry'}
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
  return (
    <div style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <AcquisitionSettings />
      <IntegrationsSettings />
      <ModuleSettings />
    </div>
  );
}
