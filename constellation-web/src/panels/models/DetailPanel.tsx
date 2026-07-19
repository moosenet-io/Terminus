// CONST-22: `models.detail` (`/models/:name`, URL-encoded full registry key) — spec §6.1.
// Four sections, each degrading INDEPENDENTLY when its source is `null` (§8: "absent sources
// null") — a brochure-only candidate renders Identity+Provenance and degrades Deployment+MINT,
// a catalog-but-evicted model renders everything but Deployment shows the exclusion reason.
import { useMemo } from 'react';
import { useParams, useNavigate, Link } from 'react-router-dom';
import { Card } from '../../components/Card';
import { Badge } from '../../components/Badge';
import type { BadgeTone } from '../../components/Badge';
import { DataTable } from '../../components/DataTable';
import { Button } from '../../components/Button';
import { SkeletonList } from '../../components/Skeleton';
import { RadarChart } from '../../viz/RadarChart';
import { ChartEmpty } from '../../viz/ChartEmpty';
import { CATEGORICAL_HEX, CHART_CHROME } from '../../viz/palette';
import { isLowConfidenceScore, mintCaveatTooltip } from '../../lib/mintCaveat';
import { useModelDetail, useMintDimensions } from '../../hooks/useModels';
import type { BrochureStatus } from '../../types/models';

const BROCHURE_TONE: Record<BrochureStatus, BadgeTone> = {
  discovered: 'neutral', evaluating: 'blue', evaluated: 'blue', shortlisted: 'violet',
  adopted: 'green', deprecated: 'amber', rejected: 'rose', archived: 'neutral',
};

function SectionCard({ title, degraded, hint, children }: { title: string; degraded?: boolean; hint?: string; children: React.ReactNode }) {
  return (
    <Card variant="content">
      <div style={{ fontSize: 13, fontWeight: 600, color: 'var(--text-100)', marginBottom: 'var(--space-3)' }}>{title}</div>
      {degraded ? <ChartEmpty height={100} message="No data for this model" hint={hint} /> : children}
    </Card>
  );
}

export function DetailPanel() {
  const { name: encoded } = useParams<{ name: string }>();
  const name = encoded ? decodeURIComponent(encoded) : undefined;
  const navigate = useNavigate();
  const { data, loading, error, notFound, refetch } = useModelDetail(name);
  const { data: mint } = useMintDimensions(name ? [name] : []);

  const radarData = useMemo(() => {
    if (!mint || mint.models.length === 0) return null;
    const model = mint.models[0];
    return mint.dimensions.map((dim, i) => ({
      dimension: dim,
      [model.model_id]: model.scores[i]?.norm ?? 0,
      fleet_median: mint.fleet_median[i] ?? 0,
    }));
  }, [mint]);

  // §6.2: "`low_confidence` and `n_samples <= 1` always render the ⚠ affordance + tooltip —
  // never silently hidden," including in `models.detail` — the radar alone only carries `norm`,
  // so this surfaces the per-dimension caveat the radar itself can't show.
  const lowConfidenceDims = useMemo(() => {
    if (!mint || mint.models.length === 0) return [];
    return mint.models[0].scores.filter(isLowConfidenceScore);
  }, [mint]);

  // §2.6: "Error (non-degraded): inline `--status-error` + retry" — a fetch/network failure
  // must render the error state, not "not found" (`error` and `notFound` are mutually
  // exclusive per `useModelDetail`, but check order still matters: a stale `data`/`notFound`
  // from a prior successful load must not mask a subsequent retry's failure).
  if (error) {
    return (
      <div style={{ padding: 'var(--space-5)', textAlign: 'center', color: 'var(--status-error)' }}>
        <div>Failed to load — {error}</div>
        <Button variant="ghost" size="sm" onClick={refetch} style={{ marginTop: 'var(--space-3)' }}>
          Retry
        </Button>
      </div>
    );
  }

  if (loading) {
    return <div style={{ padding: 'var(--space-5)' }}><SkeletonList rows={8} /></div>;
  }

  if (notFound || !data) {
    return (
      <div style={{ padding: 'var(--space-5)', textAlign: 'center', color: 'var(--text-muted)' }}>
        <div>Model not found in fleet catalog or brochure.</div>
        <Link to="/models" style={{ color: 'var(--accent-bright)' }}>← Back to Model Library</Link>
      </div>
    );
  }

  const { identity, brochure, serving, operational, catalog } = data;
  const modelId = identity?.model_name ?? name ?? '';

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-4)', padding: 'var(--space-5)', overflowY: 'auto', height: '100%' }}>
      <div>
        <Link to="/models" style={{ color: 'var(--text-muted)', fontSize: 'var(--fs-xs)' }}>← Model Library</Link>
        <h1 style={{ fontSize: 'var(--fs-h2)', color: 'var(--text-100)', margin: '4px 0 0', fontFamily: 'var(--font-mono)' }}>
          {modelId}
        </h1>
      </div>

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(340px, 1fr))', gap: 'var(--space-4)' }}>
        {/* 1. Identity */}
        <SectionCard title="Identity" degraded={!identity} hint="no advisor/identity record for this model">
          {identity && (
            <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-3)' }}>
              <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-muted)' }}>
                {identity.family ?? '—'}{identity.params_b != null ? ` · ${identity.params_b}B params` : ''}
              </div>
              {identity.quants.length > 0 && (
                <DataTable
                  columns={[
                    { key: 'quant', header: 'Quant', render: r => r.quant },
                    { key: 'vram', header: 'VRAM', align: 'right', render: r => `${r.vram_gb.toFixed(1)} GB` },
                    { key: 'penalty', header: 'Quality penalty', align: 'right', render: r => `${(r.quality_penalty * 100).toFixed(1)}%` },
                  ]}
                  rows={identity.quants}
                  rowKey={r => r.quant}
                />
              )}
              <div>
                <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', marginBottom: 4 }}>Best for</div>
                <div style={{ display: 'flex', flexWrap: 'wrap', gap: 4 }}>
                  {identity.best_for.length === 0
                    ? <span style={{ color: 'var(--text-faint)', fontSize: 'var(--fs-xs)' }}>none noted</span>
                    : identity.best_for.map(t => <Badge key={t} tone="green">{t}</Badge>)}
                </div>
              </div>
              <div>
                <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', marginBottom: 4 }}>Avoid for</div>
                <div style={{ display: 'flex', flexWrap: 'wrap', gap: 4 }}>
                  {identity.avoid_for.length === 0
                    ? <span style={{ color: 'var(--text-faint)', fontSize: 'var(--fs-xs)' }}>none noted</span>
                    : identity.avoid_for.map(t => <Badge key={t} tone="rose">{t}</Badge>)}
                </div>
              </div>
              {identity.notes && <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-body)' }}>{identity.notes}</div>}
            </div>
          )}
        </SectionCard>

        {/* 2. Provenance */}
        <SectionCard title="Provenance" degraded={!brochure} hint="not in the HF discovery brochure">
          {brochure && (
            <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-3)' }}>
              <div style={{ display: 'flex', gap: 'var(--space-2)', alignItems: 'center', flexWrap: 'wrap' }}>
                <Badge tone={BROCHURE_TONE[brochure.status]}>{brochure.status}</Badge>
                {brochure.category && <Badge tone="neutral">{brochure.category}</Badge>}
                {brochure.discovery_score != null && (
                  <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', fontVariantNumeric: 'tabular-nums' }}>
                    score {Math.round(brochure.discovery_score * 100)}
                  </span>
                )}
              </div>
              {brochure.hf_repo && (
                <a href={brochure.hf_repo} target="_blank" rel="noreferrer" style={{ color: 'var(--accent-bright)', fontSize: 'var(--fs-sm)', wordBreak: 'break-all' }}>
                  {brochure.hf_repo}
                </a>
              )}
              <div>
                <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', marginBottom: 6 }}>State timeline</div>
                <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                  {brochure.timeline.map((t, i) => (
                    <div key={i} style={{ display: 'flex', gap: 'var(--space-2)', fontSize: 'var(--fs-xs)', alignItems: 'baseline' }}>
                      <span style={{ color: 'var(--text-faint)', fontFamily: 'var(--font-mono)', minWidth: 90 }}>
                        {new Date(t.at).toLocaleDateString()}
                      </span>
                      <Badge tone={BROCHURE_TONE[t.status]} mono>{t.status}</Badge>
                      {t.note && <span style={{ color: 'var(--text-muted)' }}>{t.note}</span>}
                    </div>
                  ))}
                </div>
              </div>
              {brochure.rationale && <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-body)' }}>{brochure.rationale}</div>}
            </div>
          )}
        </SectionCard>

        {/* 3. Deployment */}
        <SectionCard title="Deployment" degraded={!serving || serving.length === 0} hint="not currently deployed anywhere">
          {serving && serving.length > 0 && (
            <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-3)' }}>
              {serving.map((s, i) => (
                <div key={i} style={{ border: '1px solid var(--border)', borderRadius: 'var(--radius-md)', padding: 'var(--space-3)' }}>
                  <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
                    <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--text-100)', fontSize: 'var(--fs-sm)' }}>{s.backend_tag}</span>
                    {s.keep_warm
                      ? <Badge tone="green" glowDot>keep-warm</Badge>
                      : <Badge tone="neutral">idle</Badge>}
                  </div>
                  <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(90px, 1fr))', gap: 'var(--space-2)', marginTop: 'var(--space-2)', fontSize: 'var(--fs-xs)' }}>
                    <div><span style={{ color: 'var(--text-muted)' }}>Runtime</span><br />{s.best_runtime}</div>
                    <div><span style={{ color: 'var(--text-muted)' }}>tok/s</span><br /><span style={{ fontVariantNumeric: 'tabular-nums' }}>{s.tok_s}</span></div>
                    <div><span style={{ color: 'var(--text-muted)' }}>VRAM peak</span><br /><span style={{ fontVariantNumeric: 'tabular-nums' }}>{s.vram_peak_gb} GB</span></div>
                    <div><span style={{ color: 'var(--text-muted)' }}>Cold load</span><br /><span style={{ fontVariantNumeric: 'tabular-nums' }}>{s.cold_load_s}s</span></div>
                  </div>
                  {s.exclusion_reason !== 'none' && (
                    <div style={{ marginTop: 'var(--space-2)' }}>
                      <Badge tone="rose">excluded: {s.exclusion_reason}</Badge>
                    </div>
                  )}
                </div>
              ))}
              {operational && (
                <div style={{ borderTop: '1px solid var(--border)', paddingTop: 'var(--space-3)' }}>
                  <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', marginBottom: 6 }}>Operational profile</div>
                  <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(100px, 1fr))', gap: 'var(--space-2)', fontSize: 'var(--fs-xs)' }}>
                    <div><span style={{ color: 'var(--text-muted)' }}>Max ctx (safe)</span><br /><span style={{ fontVariantNumeric: 'tabular-nums' }}>{operational.max_context_safe.toLocaleString()}</span></div>
                    <div><span style={{ color: 'var(--text-muted)' }}>Max ctx (absolute)</span><br /><span style={{ fontVariantNumeric: 'tabular-nums' }}>{operational.max_context_absolute.toLocaleString()}</span></div>
                    <div><span style={{ color: 'var(--text-muted)' }}>Degradation point</span><br /><span style={{ fontVariantNumeric: 'tabular-nums' }}>{operational.degradation_point?.toLocaleString() ?? '—'}</span></div>
                    <div>
                      <span style={{ color: 'var(--text-muted)' }}>Tier</span><br />
                      <Badge tone={operational.tier === 'hot' ? 'rose' : operational.tier === 'warm' ? 'amber' : 'blue'}>{operational.tier}</Badge>
                    </div>
                  </div>
                  <div style={{ display: 'flex', gap: 2, alignItems: 'flex-end', height: 32, marginTop: 'var(--space-2)' }} title="throughput strip across increasing context">
                    {operational.throughput_strip.map((v, i) => {
                      const max = Math.max(...operational.throughput_strip, 1);
                      return <div key={i} style={{ flex: 1, height: `${(v / max) * 100}%`, background: 'var(--seq-4)', borderRadius: 2 }} />;
                    })}
                  </div>
                </div>
              )}
            </div>
          )}
        </SectionCard>

        {/* 4. MINT profile */}
        <SectionCard title="MINT profile" degraded={!radarData} hint="no MINT run history for this model">
          {radarData && (
            <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-3)' }}>
              <RadarChart
                data={radarData}
                indexBy="dimension"
                series={[
                  { id: modelId, color: CATEGORICAL_HEX[0] },
                  { id: 'fleet_median', color: CHART_CHROME.deemphasis },
                ]}
                height={220}
              />
              {lowConfidenceDims.length > 0 && mint && (
                <div style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
                  <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--status-warning)' }}>
                    ⚠ {lowConfidenceDims.length} of {mint.dimensions.length} dimension{lowConfidenceDims.length === 1 ? '' : 's'} low-confidence
                    (small sample or high variance) — treat as indicative only.
                  </div>
                  <div style={{ display: 'flex', flexWrap: 'wrap', gap: 4 }}>
                    {lowConfidenceDims.map(s => (
                      <span
                        key={s.dimension}
                        title={mintCaveatTooltip(s)}
                        style={{
                          display: 'inline-flex', alignItems: 'center', gap: 4,
                          fontSize: 'var(--fs-xs)', color: 'var(--status-warning)',
                          border: '1px solid var(--status-warning)', borderRadius: 'var(--radius-sm)',
                          padding: '2px 6px',
                        }}
                      >
                        ⚠ {s.dimension}
                      </span>
                    ))}
                  </div>
                </div>
              )}
              {catalog?.card.best_pass_rate != null && (
                <div style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)' }}>
                  Best pass-rate {Math.round(catalog.card.best_pass_rate * 100)}%
                  {catalog.card.last_run_at ? ` · last run ${new Date(catalog.card.last_run_at).toLocaleDateString()}` : ''}
                </div>
              )}
              <div>
                <Button variant="ghost" size="sm" onClick={() => navigate(`/mint?model=${encodeURIComponent(modelId)}`)}>
                  Open in MINT →
                </Button>
              </div>
            </div>
          )}
        </SectionCard>
      </div>
    </div>
  );
}
