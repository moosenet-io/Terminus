// CGUI-09 (TERM #532): per-model detail. Driven by the CGUI-08 data client
// `client.models.model(name)` (GET /api/terminus/models/:name, CONST-21 + CGUI-07) — the
// client URL-encodes the name, so HF repo ids with a `/` (e.g. `org/model`) resolve
// correctly. Renders a dimension radar of the model's per-category pass rates plus its
// identity / brochure / serving / operational facts and a per-category metrics table.
//
// Fail-open throughout: a 404 or a network error degrades to an inline "unavailable" notice
// (never a thrown/blank panel); a model with no profile rows yet renders empty chart/table
// states rather than a degenerate one-spoke radar.
import { useEffect, useState } from 'react';
import { Card, CardTitle } from '../../components/Card';
import { Badge } from '../../components/Badge';
import { StatusPill } from '../../components/StatusPill';
import { Button } from '../../components/Button';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import { ChartCard } from '../../viz/ChartCard';
import { RadarChart } from '../../viz/RadarChart';
import { getAggregationClient } from '../../lib/aggregationClient';
import type { ModelListEntry, ModelDetailResponse, ModelCatalogCell } from '../../types/mint';
import { deriveServingState, deriveCostTier, coverageBadges, buildCategoryRadar, fmtPct, fmtNum, fmtGb } from './modelsData';

const RADAR_HEIGHT = 320;

interface ModelDetailViewProps {
  /** The roster row that was clicked — gives header badges immediately while detail loads. */
  entry: ModelListEntry;
  onBack: () => void;
}

/** A labelled key→value row grid, tokens only. */
function FactGrid({ rows }: { rows: Array<[string, string]> }) {
  return (
    <div style={{ display: 'grid', gridTemplateColumns: 'auto 1fr', gap: 'var(--space-1) var(--space-3)', fontSize: 'var(--fs-sm)' }}>
      {rows.map(([label, value]) => (
        <div key={label} style={{ display: 'contents' }}>
          <span style={{ color: 'var(--text-muted)' }}>{label}</span>
          <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-secondary)', textAlign: 'right', wordBreak: 'break-all' }}>
            {value}
          </span>
        </div>
      ))}
    </div>
  );
}

export function ModelDetailView({ entry, onBack }: ModelDetailViewProps) {
  const [detail, setDetail] = useState<ModelDetailResponse | null>(null);
  const [state, setState] = useState<'loading' | 'ready' | 'error'>('loading');

  useEffect(() => {
    let cancelled = false;
    setState('loading');
    setDetail(null);
    getAggregationClient()
      .models.model(entry.model_name) // client URL-encodes `/` in HF repo ids
      .then(d => { if (!cancelled) { setDetail(d); setState('ready'); } })
      .catch(() => { if (!cancelled) setState('error'); });
    return () => { cancelled = true; };
  }, [entry.model_name]);

  const serving = deriveServingState(entry);
  const tier = deriveCostTier(entry);
  const caps = coverageBadges(entry);
  const radar = buildCategoryRadar(detail);

  const catalogCells: ModelCatalogCell[] = detail?.catalog?.cells ?? [];
  const cellColumns: DataTableColumn<ModelCatalogCell>[] = [
    { key: 'test_type', header: 'Suite', render: c => <span style={{ color: 'var(--text-secondary)' }}>{c.test_type}</span> },
    { key: 'task_category', header: 'Category', render: c => <code style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--accent)' }}>{c.task_category}</code> },
    { key: 'quant', header: 'Quant', align: 'right', render: c => <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-muted)' }}>{c.quant ?? '—'}</span> },
    { key: 'pass_rate', header: 'Pass', align: 'right', render: c => <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-primary)' }}>{fmtPct(c.pass_rate)}</span> },
    { key: 'n_samples', header: 'n', align: 'right', render: c => <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-muted)' }}>{fmtNum(c.n_samples)}</span> },
    { key: 'status', header: 'Status', render: c => <Badge tone={c.low_confidence ? 'amber' : c.status === 'run' ? 'green' : 'neutral'}>{c.low_confidence ? 'low-conf' : c.status}</Badge> },
  ];

  const identity = detail?.identity;
  const brochure = detail?.brochure;
  const operational = detail?.operational;

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      {/* Header row — back + name + posture badges. */}
      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-3)', flexWrap: 'wrap' }}>
        <Button variant="ghost" onClick={onBack}>‹ Roster</Button>
        <span style={{ fontSize: 'var(--fs-h3)', fontWeight: 'var(--fw-semibold)', color: 'var(--text-primary)', fontFamily: 'var(--font-mono)' }}>
          {entry.model_name}
        </span>
        <StatusPill state={serving.state} label={serving.label} pulse={serving.pulse} />
        <Badge tone={tier.tone} mono>{tier.label}</Badge>
        {caps.map(c => <Badge key={c.key} tone="violet" dot>{c.label}</Badge>)}
      </div>

      {state === 'error' && (
        <Card variant="content">
          <div style={{ padding: 'var(--space-5)', textAlign: 'center', color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>
            Model detail unavailable — the models endpoint returned an error for
            {' '}<code style={{ fontFamily: 'var(--font-mono)' }}>{entry.model_name}</code>.
          </div>
        </Card>
      )}

      {/* Radar — per-category pass-rate profile. Empty state when nothing scored yet. */}
      <ChartCard
        title="Dimension Profile"
        subtitle="Per-category pass rate across the model's benchmarked task categories"
        height={RADAR_HEIGHT}
        loading={state === 'loading'}
        empty={state !== 'loading' && !radar.hasData}
        emptyMessage="No profile data yet"
        emptyHint="this model has no scored benchmark categories in the current epoch"
      >
        {radar.hasData && <RadarChart axes={radar.axes} height={RADAR_HEIGHT} />}
      </ChartCard>

      {/* Fact cards — identity / brochure / serving / operational. */}
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(260px, 1fr))', gap: 'var(--space-3)' }}>
        <Card variant="content">
          <CardTitle subtitle="Model family & architecture">Identity</CardTitle>
          {identity ? (
            <FactGrid rows={[
              ['Family', identity.family],
              ['Params', fmtNum(identity.params_b, 'B')],
              ['Active', fmtNum(identity.active_b, 'B')],
              ['Architecture', identity.architecture ?? '—'],
              ['Quants', Object.keys(identity.quants).join(', ') || '—'],
              ['Ollama', identity.ollama_name ?? '—'],
            ]} />
          ) : <span style={{ color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>{state === 'loading' ? 'Loading…' : 'No identity record'}</span>}
        </Card>

        <Card variant="content">
          <CardTitle subtitle="Discovery brochure record">Brochure</CardTitle>
          {brochure ? (
            <FactGrid rows={[
              ['HF Repo', brochure.hf_repo ?? '—'],
              ['Category', brochure.category],
              ['Status', brochure.status],
              ['Discovery', brochure.discovery_score == null ? '—' : brochure.discovery_score.toFixed(2)],
              ['VRAM', fmtGb(brochure.vram_footprint_gb)],
              ['gfx1151', brochure.gfx1151_class ?? '—'],
            ]} />
          ) : <span style={{ color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>{state === 'loading' ? 'Loading…' : 'Not in brochure'}</span>}
        </Card>

        <Card variant="content" glow={entry.serving_now}>
          <CardTitle subtitle="Keep-warm serving profile">Serving</CardTitle>
          {detail && detail.serving.length > 0 ? (
            <FactGrid rows={detail.serving.flatMap(s => [
              ['Backend', s.backend_tag],
              ['Runtime', s.best_runtime ?? '—'],
              ['Throughput', s.tok_s == null ? '—' : `${s.tok_s.toFixed(1)} tok/s`],
              ['Peak', fmtGb(s.vram_or_ram_peak_gb)],
              ['Cold load', s.cold_load_s == null ? '—' : `${s.cold_load_s.toFixed(1)}s`],
              ['Keep-warm', s.keep_warm ? 'yes' : 'no'],
            ] as Array<[string, string]>)} />
          ) : <span style={{ color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>{state === 'loading' ? 'Loading…' : 'Not currently profiled for serving'}</span>}
        </Card>

        <Card variant="content">
          <CardTitle subtitle="Context & throughput operating envelope">Operational</CardTitle>
          {operational ? (
            <FactGrid rows={[
              ['Tier', operational.overall_tier ?? '—'],
              ['Ctx safe', fmtNum(operational.max_context_safe)],
              ['Ctx max', fmtNum(operational.max_context_absolute)],
              ['tok/s @8k', fmtNum(operational.throughput_at_8k)],
              ['tok/s @32k', fmtNum(operational.throughput_at_32k)],
              ['Build timeout', operational.recommended_timeout_build_sec == null ? '—' : `${operational.recommended_timeout_build_sec}s`],
            ]} />
          ) : <span style={{ color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>{state === 'loading' ? 'Loading…' : 'No operational profile'}</span>}
        </Card>
      </div>

      {/* Per-category metrics table. */}
      <Card variant="content">
        <CardTitle subtitle="Every benchmarked cell for this model in the current epoch">Category Metrics</CardTitle>
        <DataTable
          columns={cellColumns}
          rows={catalogCells}
          rowKey={(c, i) => `${c.test_type}-${c.task_category}-${c.quant ?? 'na'}-${i}`}
          emptyMessage={state === 'loading' ? 'Loading…' : 'No benchmarked categories yet'}
        />
      </Card>
    </div>
  );
}
