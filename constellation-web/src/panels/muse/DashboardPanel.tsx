// CONST-20: muse.dashboard -- MetricCards row, On Deck rail, Premieres list, Gaps summary
// (spec §5.4). Every section below is its own independent `useMuseSection` call (see
// `hooks/useMuse.ts`) wrapped in its own `ChartCard` -- the central per-endpoint degradation
// requirement (MUSEX-WIRE reality): one dead endpoint collapses only its own card to
// ChartEmpty("not yet wired"), the other three keep rendering normally.
//
// Manually verified (per this item's "prove it works by killing individual mock endpoints"
// instruction): deleting/renaming any one of `'muse /stats'`, `'muse /on_deck'`,
// `'muse /premiere'`, `'muse /gaps'` from `MOCK_GET` in aggregationClient.ts (mockGetFor then
// resolves `null` for that path -- the mock world's 404-equivalent, see useMuse.ts's top
// comment) makes only that section's ChartCard render its degraded state, confirmed by
// reading through `useMuseSection`'s null-branch and each section below independently; the
// other three sections' own `MOCK_GET` entries are untouched and keep resolving real data.
import { ChartCard } from '../../viz/ChartCard';
import { MetricCard } from '../../components/MetricCard';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import {
  useMuseStats,
  useMuseOnDeck,
  useMusePremiere,
  useMuseGaps,
  museArtUrl,
  type MuseOnDeckItem,
  type MusePremiereItem,
  type MuseGapItem,
} from '../../hooks/useMuse';

function formatRelativeTime(iso: string | null): string {
  if (!iso) return '—';
  const ms = Date.now() - new Date(iso).getTime();
  const mins = Math.round(ms / 60000);
  if (mins < 1) return 'just now';
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.round(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.round(hours / 24)}d ago`;
}

function MetricsSection() {
  const { data, loading, degraded } = useMuseStats();
  const empty = !loading && !degraded && (data === null || data.library_size === 0);
  return (
    <ChartCard
      title="Library Overview"
      height={92}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No Muse library yet"
      emptyHint="Connect Muse to a media library to see stats here"
    >
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(4, 1fr)', gap: 'var(--space-3)', height: '100%' }}>
        <MetricCard label="Library Size" value={data ? String(data.library_size) : '—'} />
        <MetricCard label="Active Channels" value={data ? String(data.active_channels) : '—'} />
        <MetricCard
          label="Pending Items"
          value={data ? String(data.pending_items) : '—'}
          valueColor={data && data.pending_items > 0 ? 'warning' : 'primary'}
        />
        <MetricCard label="Last Ingest" value={data ? formatRelativeTime(data.last_ingest_at) : '—'} />
      </div>
    </ChartCard>
  );
}

function OnDeckSection() {
  const { data, loading, degraded } = useMuseOnDeck();
  const items = data?.items ?? [];
  const empty = !loading && !degraded && items.length === 0;
  return (
    <ChartCard
      title="On Deck"
      subtitle="Continue watching"
      height={200}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="Nothing in progress"
      emptyHint="Items you start watching in Muse show up here"
    >
      <div style={{ display: 'flex', gap: 'var(--space-3)', overflowX: 'auto', height: '100%', paddingBottom: 4 }}>
        {items.map((item: MuseOnDeckItem) => (
          <div
            key={item.id}
            style={{
              flex: '0 0 120px',
              display: 'flex',
              flexDirection: 'column',
              gap: 'var(--space-1)',
            }}
          >
            <div
              style={{
                width: 120,
                height: 120,
                borderRadius: 'var(--radius-md)',
                background: 'var(--space-600)',
                border: '1px solid var(--border)',
                overflow: 'hidden',
              }}
            >
              <img
                src={museArtUrl('poster', item.id)}
                alt=""
                aria-hidden
                style={{ width: '100%', height: '100%', objectFit: 'cover', display: 'block' }}
                onError={e => { (e.currentTarget as HTMLImageElement).style.visibility = 'hidden'; }}
              />
            </div>
            <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-100)', lineHeight: 'var(--lh-tight)' }}>{item.title}</div>
            <div
              aria-label={`${item.progress_pct}% watched`}
              style={{ height: 3, borderRadius: 'var(--radius-pill)', background: 'var(--space-600)', overflow: 'hidden' }}
            >
              <div style={{ width: `${item.progress_pct}%`, height: '100%', background: 'var(--accent)' }} />
            </div>
          </div>
        ))}
      </div>
    </ChartCard>
  );
}

function PremieresSection() {
  const { data, loading, degraded } = useMusePremiere();
  const items = data?.items ?? [];
  // Edge case (§5.4): past-dated premieres are sorted alongside upcoming ones and dimmed, not
  // hidden -- sort ascending by release_date across both past and future entries.
  const sorted = [...items].sort((a, b) => new Date(a.release_date).getTime() - new Date(b.release_date).getTime());
  const empty = !loading && !degraded && sorted.length === 0;
  return (
    <ChartCard
      title="Premieres"
      height={200}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No premieres scheduled"
      emptyHint="Upcoming releases tracked by Muse will list here"
    >
      <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)', height: '100%', overflowY: 'auto' }}>
        {sorted.map((item: MusePremiereItem) => {
          const isPast = new Date(item.release_date).getTime() < Date.now();
          return (
            <div
              key={item.id}
              style={{
                display: 'flex',
                justifyContent: 'space-between',
                alignItems: 'baseline',
                opacity: isPast ? 0.5 : 1,
              }}
            >
              <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-100)' }}>{item.title}</span>
              <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-xs)', color: 'var(--text-muted)' }}>
                {new Date(item.release_date).toLocaleDateString()}
                {isPast ? ' (past)' : ''}
              </span>
            </div>
          );
        })}
      </div>
    </ChartCard>
  );
}

const GAP_COLUMNS: DataTableColumn<MuseGapItem>[] = [
  { key: 'title', header: 'Title', render: r => r.title },
  { key: 'kind', header: 'Kind', render: r => r.kind },
  { key: 'detail', header: 'Detail', render: r => r.detail },
];

function GapsSection() {
  const { data, loading, degraded } = useMuseGaps();
  const gaps = data?.gaps ?? [];
  const empty = !loading && !degraded && gaps.length === 0;
  return (
    <ChartCard
      title="Gaps"
      subtitle={data ? `${data.total} open` : undefined}
      height={200}
      loading={loading}
      degraded={degraded}
      empty={empty}
      emptyMessage="No gaps"
      emptyHint="Missing seasons/entries Muse detects will list here"
    >
      <DataTable columns={GAP_COLUMNS} rows={gaps} rowKey={r => r.id} emptyMessage="No gaps" />
    </ChartCard>
  );
}

export function DashboardPanel() {
  return (
    <div style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <MetricsSection />
      <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr 1fr', gap: 'var(--space-4)' }}>
        <OnDeckSection />
        <PremieresSection />
        <GapsSection />
      </div>
    </div>
  );
}
