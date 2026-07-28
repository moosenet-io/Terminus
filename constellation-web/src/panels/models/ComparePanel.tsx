// `models.compare` (`/models/compare?m=a&m=b…`, 2-4 models, URL state ONLY — no `client.prefs`
// entry, per the original CONST-22 spec §6.1) — side-by-side DataTable, MINT radar overlay
// (≤4 series), and a Pareto scatter (VRAM vs. best pass-rate) with compared models emphasized
// and the rest of the fleet de-emphasized via `--chart-deemphasis`.
//
// RECONCILIATION NOTE: this panel was originally built (CONST-22) against a bespoke mock data
// layer — `hooks/useModels.ts` + `types/models.ts` — that never wired to the real backend. The
// Models module's list/detail surface that DID land and get wired to the live Terminus models
// API (CGUI-09/TERM #532: `RosterPanel.tsx` + `ModelDetailView.tsx`, `types/mint.ts`,
// `getAggregationClient().models.*`) has no compare feature at all, so this panel is kept as a
// pure ADDITION — but rewritten to fetch through that same real data client instead of the
// bespoke hooks, and its `CompareRow` value-extraction adapted field-by-field to the REAL
// `ModelDetailResponse` shape (`types/mint.ts`, 1:1 with `models_api.rs`):
//   - VRAM peak (GB): real `ModelServingRow.vram_or_ram_peak_gb` (was the bespoke
//     `serving.vram_peak_gb` / `identity.quants[0].vram_gb` — the real `ModelIdentity.quants` is
//     a `Record<string, ModelQuantInfo>`, not an array, so there's no "first quant" to index).
//     Falls back to the roster's `ModelListEntry.vram_gb` (already fetched for the Pareto
//     background) when no serving row exists yet — same fallback RosterPanel/ModelDetailView use
//     via `deriveCostTier`'s footprint logic.
//   - Max context (safe): `ModelOperationalProfile.max_context_safe` — same field name, unchanged.
//   - tok/s, Cold load (s): `ModelServingRow.tok_s` / `.cold_load_s` — same field names, now off
//     `serving[0]` (an array of rows, same as the bespoke shape).
//   - Best pass-rate: the real `ModelCatalogDetail.card` has NO `best_pass_rate` field (unlike
//     the bespoke mock's `catalog.card.best_pass_rate`) — the real backend only exposes it on the
//     roster row (`ModelListEntry.best_pass_rate`, CONST-21/CGUI-07). Sourced from the same
//     fleet-roster fetch used for the Pareto background rather than fabricating a field that
//     doesn't exist on the detail response.
// MINT dimension overlay: `client.mint.dimensions({ models })` returns the real
// `MintDimensionsResponse` (`dimension`/`norm`/`raw`/`std_dev`/`n`/`low_confidence` — identical
// field names to the bespoke shape this was built against, so `mintScoreFor` needed no changes).
// The N-series nivo radar itself is a new small viz-kit component (`CompareRadarChart`) rather
// than the CGUI-09 `RadarChart` (single-series-only, `axes: RadarAxis[]`) or `RadarChartKit`
// (single + one de-emphasis series) — neither shape fits an up-to-4-model overlay; see
// `viz/CompareRadarChart.tsx`'s header comment.
import { useEffect, useMemo, useRef, useState } from 'react';
import { useSearchParams, Link } from 'react-router-dom';
import { Card } from '../../components/Card';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import { SkeletonList } from '../../components/Skeleton';
import { CompareRadarChart } from '../../viz/CompareRadarChart';
import { ChartCard } from '../../viz/ChartCard';
import { ChartTooltip } from '../../viz/ChartTooltip';
import {
  ScatterChart, Scatter, XAxis, YAxis, ZAxis, CartesianGrid, Tooltip, ResponsiveContainer,
} from '../../viz/recharts';
import { rechartsGridProps, rechartsTickStyle } from '../../viz/theme';
import { SlotAssigner, CHART_CHROME } from '../../viz/palette';
import { isLowConfidenceScore, mintCaveatTooltip } from '../../lib/mintCaveat';
import { getAggregationClient } from '../../lib/aggregationClient';
import type { ModelDetailResponse, ModelListEntry, MintDimensionsResponse } from '../../types/mint';

const MAX_COMPARE = 4;

/** `?m=a&m=b…` — URL state only (no `client.prefs` entry), capped at {@link MAX_COMPARE}. */
function parseCompareModels(searchParams: URLSearchParams): string[] {
  return searchParams.getAll('m').filter(Boolean).slice(0, MAX_COMPARE);
}

interface CompareRow {
  key: string;
  label: string;
  direction: 'min' | 'max' | null;
  format: (v: number) => string;
  valueFor: (d: ModelDetailResponse | undefined, roster: ModelListEntry | undefined) => number | null;
}

function buildStaticRows(): CompareRow[] {
  return [
    {
      key: 'vram', label: 'VRAM peak (GB)', direction: 'min',
      format: v => v.toFixed(1),
      valueFor: (d, roster) => d?.serving?.[0]?.vram_or_ram_peak_gb ?? roster?.vram_gb ?? null,
    },
    {
      key: 'context', label: 'Max context (safe)', direction: 'max',
      format: v => v.toLocaleString(),
      valueFor: d => d?.operational?.max_context_safe ?? null,
    },
    {
      key: 'tok_s', label: 'tok/s', direction: 'max',
      format: v => v.toFixed(1),
      valueFor: d => d?.serving?.[0]?.tok_s ?? null,
    },
    {
      key: 'cold_load', label: 'Cold load (s)', direction: 'min',
      format: v => v.toFixed(0),
      valueFor: d => d?.serving?.[0]?.cold_load_s ?? null,
    },
    {
      key: 'pass_rate', label: 'Best pass-rate', direction: 'max',
      format: v => `${Math.round(v * 100)}%`,
      // Not present on the real ModelDetailResponse's catalog card — sourced from the roster row.
      valueFor: (_d, roster) => roster?.best_pass_rate ?? null,
    },
  ];
}

/** Fetch full detail records for 2-4 compared models together (a variable-length array of
 *  per-model hooks would violate rules-of-hooks, so this is one effect + `Promise.all` over the
 *  real `client.models.model()` method — the same method ModelDetailView uses for a single
 *  model). A 404/error for one model degrades that model's columns to `—` rather than failing
 *  the whole comparison. */
function useModelsDetails(names: string[]): { data: Record<string, ModelDetailResponse>; loading: boolean } {
  const [data, setData] = useState<Record<string, ModelDetailResponse>>({});
  const [loading, setLoading] = useState(true);
  const key = names.join(',');

  useEffect(() => {
    if (names.length === 0) {
      setData({});
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    const client = getAggregationClient();
    Promise.all(
      names.map(async n => {
        try {
          const res = await client.models.model(n);
          return [n, res] as const;
        } catch {
          return [n, null] as const;
        }
      }),
    ).then(entries => {
      if (cancelled) return;
      const out: Record<string, ModelDetailResponse> = {};
      for (const [n, res] of entries) if (res) out[n] = res;
      setData(out);
      setLoading(false);
    });
    return () => { cancelled = true; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  return { data, loading };
}

export function ComparePanel() {
  const [searchParams] = useSearchParams();
  const names = useMemo(() => parseCompareModels(searchParams), [searchParams]);
  const slotAssigner = useRef(new SlotAssigner()).current;

  const { data: details, loading } = useModelsDetails(names);
  const [mint, setMint] = useState<MintDimensionsResponse | null>(null);
  const [fleetList, setFleetList] = useState<ModelListEntry[] | null>(null);

  useEffect(() => {
    if (names.length === 0) { setMint(null); return; }
    let cancelled = false;
    getAggregationClient().mint.dimensions({ models: names })
      .then(res => { if (!cancelled) setMint(res); })
      .catch(() => { if (!cancelled) setMint(null); });
    return () => { cancelled = true; };
  }, [names]);

  useEffect(() => {
    let cancelled = false;
    getAggregationClient().models.list({ scope: 'all', limit: 500 })
      .then(res => { if (!cancelled) setFleetList(res.models); })
      .catch(() => { if (!cancelled) setFleetList([]); });
    return () => { cancelled = true; };
  }, []);

  if (names.length < 2) {
    return (
      <div style={{ padding: 'var(--space-6)', textAlign: 'center', color: 'var(--text-muted)' }}>
        <div>Select 2–4 models on the Model Library to compare.</div>
        <Link to="/models/roster" style={{ color: 'var(--accent-bright)' }}>→ Model Library</Link>
      </div>
    );
  }

  if (loading) {
    return <div style={{ padding: 'var(--space-5)' }}><SkeletonList rows={6} /></div>;
  }

  const rosterByName = new Map((fleetList ?? []).map(m => [m.model_name, m]));
  const colorFor = (name: string) => slotAssigner.colorFor(name);
  const staticRows = buildStaticRows();

  // MINT dimension rows read via `mintScoreFor(modelName, dim)` in the table renderer below
  // (keyed by model NAME, not the detail record) — `__dim` flags a row as one of these so the
  // renderer branches to that lookup instead of `row.valueFor(detail, roster)`.
  const mintDimensionRows: (CompareRow & { __dim: string })[] = (mint?.dimensions ?? []).map(dim => ({
    key: `mint-${dim}`,
    label: dim,
    direction: 'max',
    format: (v: number) => `${Math.round(v * 100)}%`,
    valueFor: () => null,
    __dim: dim,
  }));

  function mintScoreFor(modelName: string, dim: string) {
    const m = mint?.models.find(mm => mm.model_id === modelName);
    return m?.scores.find(s => s.dimension === dim);
  }

  const tableColumns: DataTableColumn<{ row: CompareRow & { __dim?: string } }>[] = [
    { key: 'metric', header: 'Metric', render: ({ row }) => <span style={{ color: 'var(--text-muted)' }}>{row.label}</span> },
    ...names.map(n => ({
      key: n,
      header: n,
      align: 'right' as const,
      render: ({ row }: { row: CompareRow & { __dim?: string } }) => {
        const d = details[n];
        if (row.__dim) {
          const score = mintScoreFor(n, row.__dim);
          if (!score || score.norm == null) return <span style={{ color: 'var(--text-faint)' }}>—</span>;
          const values = names
            .map(m => mintScoreFor(m, row.__dim as string)?.norm)
            .filter((v): v is number => v != null);
          const best = row.direction === 'max' ? Math.max(...values) : Math.min(...values);
          const isBest = score.norm === best;
          return (
            <span style={{
              fontVariantNumeric: 'tabular-nums',
              outline: isBest ? '1px solid var(--accent-bright)' : 'none',
              outlineOffset: 2, borderRadius: 4, padding: '1px 4px',
              display: 'inline-flex', alignItems: 'center', gap: 4,
            }}>
              {row.format(score.norm)}
              {isLowConfidenceScore(score) && (
                <span title={mintCaveatTooltip(score)} style={{ color: 'var(--status-warning)' }}>⚠</span>
              )}
            </span>
          );
        }
        const roster = rosterByName.get(n);
        const value = row.valueFor(d, roster);
        if (value == null) return <span style={{ color: 'var(--text-faint)' }}>—</span>;
        const values = names
          .map(m => row.valueFor(details[m], rosterByName.get(m)))
          .filter((v): v is number => v != null);
        const best = row.direction === 'max' ? Math.max(...values) : row.direction === 'min' ? Math.min(...values) : null;
        const isBest = best != null && value === best;
        return (
          <span style={{
            fontVariantNumeric: 'tabular-nums',
            outline: isBest ? '1px solid var(--accent-bright)' : 'none',
            outlineOffset: 2, borderRadius: 4, padding: '1px 4px',
          }}>
            {row.format(value)}
          </span>
        );
      },
    })),
  ];

  const allRows = [...staticRows, ...mintDimensionRows];

  const radarData = mint?.dimensions.map((dim) => {
    const row: Record<string, string | number> = { dimension: dim };
    for (const n of names) row[n] = mintScoreFor(n, dim)?.norm ?? 0;
    return row;
  }) ?? null;

  const paretoBackground = (fleetList ?? [])
    .filter(m => !names.includes(m.model_name) && m.vram_gb != null && m.best_pass_rate != null)
    .map(m => ({ x: m.vram_gb as number, y: (m.best_pass_rate as number) * 100, name: m.model_name }));

  const paretoEmphasized = names
    .map(n => {
      const d = details[n];
      const roster = rosterByName.get(n);
      const vram = d?.serving?.[0]?.vram_or_ram_peak_gb ?? roster?.vram_gb;
      const passRate = roster?.best_pass_rate;
      if (vram == null || passRate == null) return null;
      return { name: n, x: vram, y: passRate * 100, color: colorFor(n) };
    })
    .filter((p): p is { name: string; x: number; y: number; color: string } => p != null);

  const tick = rechartsTickStyle();

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-4)', padding: 'var(--space-5)', overflowY: 'auto', height: '100%' }}>
      <div>
        <Link to="/models/roster" style={{ color: 'var(--text-muted)', fontSize: 'var(--fs-xs)' }}>← Model Library</Link>
        <h1 style={{ fontSize: 'var(--fs-h2)', color: 'var(--text-100)', margin: '4px 0 0' }}>
          Compare {names.length} models
        </h1>
      </div>

      <Card variant="content">
        <DataTable
          columns={tableColumns}
          rows={allRows.map(row => ({ row }))}
          rowKey={({ row }) => row.key}
        />
      </Card>

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(360px, 1fr))', gap: 'var(--space-4)' }}>
        <ChartCard title="MINT profile overlay" subtitle="normalized dimension scores" height={280} empty={!radarData}>
          {radarData && (
            <CompareRadarChart
              data={radarData}
              series={names.slice(0, MAX_COMPARE).map(n => ({ id: n, color: colorFor(n) }))}
              height={280}
            />
          )}
        </ChartCard>

        <ChartCard
          title="Pareto: VRAM vs. best pass-rate"
          subtitle="compared models emphasized; rest of the fleet de-emphasized"
          height={280}
          empty={paretoEmphasized.length === 0}
        >
          <ResponsiveContainer width="100%" height={280}>
            <ScatterChart margin={{ top: 8, right: 16, bottom: 8, left: 0 }}>
              <CartesianGrid {...rechartsGridProps()} />
              <XAxis type="number" dataKey="x" name="VRAM (GB)" tick={tick} label={{ value: 'VRAM (GB)', position: 'insideBottom', offset: -4, fill: tick.fill, fontSize: 11 }} />
              <YAxis type="number" dataKey="y" name="Best pass-rate (%)" tick={tick} />
              <ZAxis range={[60, 60]} />
              <Tooltip
                cursor={{ stroke: CHART_CHROME.axis }}
                content={({ active, payload }) => {
                  if (!active || !payload?.length) return null;
                  const p = payload[0].payload as { name: string; x: number; y: number };
                  return <ChartTooltip title={p.name} rows={[
                    { key: 'vram', label: 'VRAM', value: `${p.x.toFixed(1)} GB` },
                    { key: 'pr', label: 'Pass-rate', value: `${p.y.toFixed(0)}%` },
                  ]} />;
                }}
              />
              <Scatter data={paretoBackground} fill={CHART_CHROME.deemphasis} opacity={0.5} />
              {paretoEmphasized.map(p => (
                <Scatter key={p.name} name={p.name} data={[p]} fill={p.color} />
              ))}
            </ScatterChart>
          </ResponsiveContainer>
        </ChartCard>
      </div>
    </div>
  );
}
