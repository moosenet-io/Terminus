// CONST-20: muse.channels -- channels list + per-channel lineup, guide grid, and
// operator-gated compose/maintenance mutations (spec §5.4). Same independent per-section
// degrade boundary as the other two Muse panels (see DashboardPanel's top comment).
//
// Guide grid is rendered as a plain `DataTable` timeline (channel/title/start/end columns),
// deliberately NOT an EPG-widget layout -- spec §5.4 is explicit about this ("rendered as a
// DataTable timeline, not an EPG widget").
//
// Compose/maintenance: CONST-27 (real role claims + a shared RoleGate/ConfirmDialog) hasn't
// merged yet, so these use the CONST-20 seam components (`components/RoleGate.tsx`,
// `components/ConfirmDialog.tsx`, `hooks/useAuthRole.ts`) -- each clearly marked as a
// stand-in in its own file. There is no Toast infra yet either (CONST-26, not merged) so the
// mutation result renders as an inline status line next to the action buttons rather than a
// toast; that's a deliberate scope cut, not an oversight -- see the README's Muse section.
import { useState } from 'react';
import { ChartCard } from '../../viz/ChartCard';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import { Button } from '../../components/Button';
import { Badge } from '../../components/Badge';
import { RoleGate } from '../../components/RoleGate';
import { ConfirmDialog } from '../../components/ConfirmDialog';
import {
  useMuseChannels,
  useMuseLineup,
  useMuseGuide,
  useMuseChannelActions,
  type MuseChannel,
  type MuseLineupItem,
  type MuseGuideEntry,
} from '../../hooks/useMuse';

type PendingAction = { kind: 'compose' | 'maintenance'; channelId: string; channelName: string } | null;

function ChannelsListSection({
  selectedId,
  onSelect,
  onRequestAction,
}: {
  selectedId: string | null;
  onSelect: (channel: MuseChannel) => void;
  onRequestAction: (kind: 'compose' | 'maintenance', channel: MuseChannel) => void;
}) {
  const { data, loading, degraded } = useMuseChannels();
  const channels = data?.channels ?? [];
  const empty = !loading && !degraded && channels.length === 0;

  const columns: DataTableColumn<MuseChannel>[] = [
    {
      key: 'name',
      header: 'Channel',
      render: c => (
        <button
          type="button"
          onClick={() => onSelect(c)}
          style={{
            background: 'none',
            border: 'none',
            padding: 0,
            cursor: 'pointer',
            color: selectedId === c.id ? 'var(--accent-bright)' : 'var(--text-100)',
            fontWeight: selectedId === c.id ? 'var(--fw-semibold)' : 'var(--fw-regular)',
            textDecoration: 'underline dotted',
          }}
        >
          {c.name}
        </button>
      ),
    },
    { key: 'items', header: 'Items', align: 'right', render: c => String(c.item_count) },
    {
      key: 'actions',
      header: 'Actions',
      align: 'right',
      render: c => (
        <RoleGate minRole="operator" fallback={<Badge tone="neutral">viewer</Badge>}>
          <div style={{ display: 'flex', gap: 'var(--space-2)', justifyContent: 'flex-end' }}>
            <Button variant="secondary" size="sm" onClick={() => onRequestAction('compose', c)}>Compose</Button>
            <Button variant="ghost" size="sm" onClick={() => onRequestAction('maintenance', c)}>Maintenance</Button>
          </div>
        </RoleGate>
      ),
    },
  ];

  return (
    <ChartCard
      title="Channels"
      height={channels.length === 0 ? 120 : Math.min(60 + channels.length * 40, 320)}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No channels yet"
      emptyHint="Muse channels appear here once composed"
    >
      <DataTable columns={columns} rows={channels} rowKey={c => c.id} emptyMessage="No channels yet" />
    </ChartCard>
  );
}

function LineupSection({ channelId, channelName }: { channelId: string | null; channelName: string | null }) {
  const { data, loading, degraded } = useMuseLineup(channelId);
  const lineup = data?.lineup ?? [];
  const empty = channelId !== null && !loading && !degraded && lineup.length === 0;
  const idle = channelId === null;

  const columns: DataTableColumn<MuseLineupItem>[] = [
    { key: 'position', header: '#', align: 'right', render: r => String(r.position) },
    { key: 'title', header: 'Title', render: r => r.title },
  ];

  return (
    <ChartCard
      title="Lineup"
      subtitle={channelName ?? 'Select a channel'}
      height={200}
      loading={loading}
      degraded={degraded}
      empty={empty || idle}
      emptyMessage={idle ? 'No channel selected' : 'Empty lineup'}
      emptyHint={idle ? 'Pick a channel above to see its lineup' : 'This channel has no scheduled items yet'}
    >
      <DataTable columns={columns} rows={lineup} rowKey={r => r.id} emptyMessage="Empty lineup" />
    </ChartCard>
  );
}

const GUIDE_COLUMNS: DataTableColumn<MuseGuideEntry>[] = [
  { key: 'channel', header: 'Channel', render: r => r.channel_id },
  { key: 'title', header: 'Title', render: r => r.title },
  { key: 'start', header: 'Start', render: r => new Date(r.start).toLocaleString() },
  { key: 'end', header: 'End', render: r => new Date(r.end).toLocaleString() },
];

function GuideSection() {
  const { data, loading, degraded } = useMuseGuide();
  const entries = data?.entries ?? [];
  const empty = !loading && !degraded && entries.length === 0;
  return (
    <ChartCard
      title="Guide"
      subtitle="Timeline (table view, not an EPG grid — spec §5.4)"
      height={Math.min(60 + entries.length * 36, 280)}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No guide data yet"
      emptyHint="Scheduled programming will list here once channels have a lineup"
    >
      <DataTable columns={GUIDE_COLUMNS} rows={entries} rowKey={(r, i) => `${r.channel_id}-${i}`} emptyMessage="No guide data yet" />
    </ChartCard>
  );
}

export function ChannelsPanel() {
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [selectedName, setSelectedName] = useState<string | null>(null);
  const [pending, setPending] = useState<PendingAction>(null);
  const [busy, setBusy] = useState(false);
  const [statusMessage, setStatusMessage] = useState<string | null>(null);
  const { composeChannel, runMaintenance } = useMuseChannelActions();

  function requestAction(kind: 'compose' | 'maintenance', channel: MuseChannel) {
    setStatusMessage(null);
    setPending({ kind, channelId: channel.id, channelName: channel.name });
  }

  async function confirmAction() {
    if (!pending) return;
    setBusy(true);
    try {
      const result = pending.kind === 'compose'
        ? await composeChannel(pending.channelId)
        : await runMaintenance(pending.channelId);
      const status = (result as { status?: string } | null)?.status ?? 'ok';
      setStatusMessage(`${pending.kind} on "${pending.channelName}": ${status}`);
    } catch (err) {
      setStatusMessage(`${pending.kind} on "${pending.channelName}" failed: ${err instanceof Error ? err.message : String(err)}`);
    } finally {
      setBusy(false);
      setPending(null);
    }
  }

  return (
    <div style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      {statusMessage && (
        <div
          role="status"
          aria-live="polite"
          style={{
            fontFamily: 'var(--font-mono)',
            fontSize: 'var(--fs-sm)',
            color: 'var(--text-200)',
            background: 'var(--bg-elevated)',
            border: '1px solid var(--border)',
            borderRadius: 'var(--radius-md)',
            padding: 'var(--space-2) var(--space-3)',
          }}
        >
          {statusMessage}
        </div>
      )}
      <ChannelsListSection
        selectedId={selectedId}
        onSelect={channel => {
          setSelectedId(channel.id);
          setSelectedName(channel.name);
        }}
        onRequestAction={requestAction}
      />
      <LineupSection channelId={selectedId} channelName={selectedName} />
      <GuideSection />

      <ConfirmDialog
        open={pending !== null}
        title={pending?.kind === 'compose' ? 'Compose channel?' : 'Run channel maintenance?'}
        description={pending ? `This will queue a ${pending.kind} run for "${pending.channelName}".` : undefined}
        confirmLabel={pending?.kind === 'compose' ? 'Compose' : 'Run maintenance'}
        destructive={pending?.kind === 'maintenance'}
        busy={busy}
        onConfirm={confirmAction}
        onCancel={() => setPending(null)}
      />
    </div>
  );
}
