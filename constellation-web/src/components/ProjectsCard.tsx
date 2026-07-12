// SPOL-04: ProjectsCard — refined progress bars and row hover states.
// SGUI-06: Projects summary card for dashboard
// LIVE-03: loading skeleton, cached badge, defensive 0/0 guard.
import { useState } from 'react';
import { ProgressBar, pctColor } from './ProgressBar';
import { Skeleton } from './Skeleton';
import type { Project } from '../types/api';

interface Props {
  projects: Project[];
  cached?: boolean;
  cachedAgoSecs?: number;
  /** LIVE-03: No data available at all — render skeleton rows, never 0/0. */
  loading?: boolean;
}

function formatCacheAge(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  return `${Math.floor(secs / 3600)}h`;
}

export function ProjectsCard({ projects, cached, cachedAgoSecs, loading }: Props) {
  // LIVE-03: loading=true → skeleton shimmer rows instead of "0/0" or "No projects found"
  if (loading) {
    return (
      <div className="h-card">
        <div className="h-card-header">
          <span style={{ fontWeight: 600, fontSize: 'var(--text-md)', color: 'var(--text-primary)' }}>Projects</span>
          <span style={{ color: 'var(--text-tertiary)', fontSize: 'var(--text-xs)' }}>connecting…</span>
        </div>
        <div style={{ padding: 'var(--space-3) var(--space-4)', display: 'flex', flexDirection: 'column', gap: 'var(--space-3)' }}>
          {[1, 2, 3].map(i => (
            <div key={i} style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-3)' }}>
              <Skeleton variant="bar" width={52} height={12} />
              <Skeleton variant="bar" style={{ flex: 1 }} height={8} />
              <Skeleton variant="bar" width={32} height={12} />
            </div>
          ))}
        </div>
      </div>
    );
  }

  if (projects.length === 0) {
    return (
      <div className="h-card">
        <div className="h-card-header">
          <span style={{ fontWeight: 600, fontSize: 'var(--text-md)' }}>Projects</span>
        </div>
        <div className="h-card-body" style={{ color: 'var(--text-tertiary)', textAlign: 'center' }}>
          No projects found
        </div>
      </div>
    );
  }

  return (
    <div className="h-card">
      <div className="h-card-header" style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
        <span style={{ fontWeight: 600, fontSize: 'var(--text-md)', color: 'var(--text-primary)' }}>
          Projects
        </span>
        {cached && cachedAgoSecs !== undefined && (
          <span style={{ color: 'var(--text-tertiary)', fontSize: 'var(--text-xs)' }}
                title="Showing cached data — Plane data temporarily unavailable">
            cached {formatCacheAge(cachedAgoSecs)} ago
          </span>
        )}
      </div>
      <div>
        {projects.slice(0, 8).map(p => (
          <ProjectRow key={p.identifier} project={p} />
        ))}
      </div>
    </div>
  );
}

/** Enrichment % color: red→amber→teal→green as readiness increases. */
function enrichColor(pct: number): string {
  if (pct >= 100) return 'var(--status-success)';
  if (pct >= 60)  return 'var(--accent-primary)';
  if (pct >= 30)  return 'var(--status-warning)';
  return 'var(--status-error)';
}

function ProjectRow({ project: p }: { project: Project }) {
  const [hovered, setHovered] = useState(false);
  const total = (p.counts?.todo || 0) + (p.counts?.in_progress || 0) + (p.counts?.done || 0);
  // LIVE-03: if total is 0 we have no real data — show "—" instead of "0%"
  const hasData = total > 0;
  const pct = hasData ? (p.progress_pct ?? Math.round(p.counts.done * 100 / total)) : null;
  const ePct = p.enrichment_pct ?? 0;
  const color = pct !== null ? pctColor(pct) : 'var(--text-tertiary)';
  const eColor = enrichColor(ePct);

  return (
    <div
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
      style={{
        padding: 'var(--space-2) var(--space-4)',
        borderBottom: '1px solid var(--border-subtle)',
        display: 'flex',
        alignItems: 'center',
        gap: 'var(--space-3)',
        background: hovered ? 'var(--bg-surface-raised)' : 'transparent',
        transition: `background var(--transition-fast)`,
        cursor: 'default',
      }}
    >
      {/* Identifier */}
      <span style={{
        fontFamily: 'var(--font-mono)',
        fontSize: 'var(--text-xs)',
        color: 'var(--accent-primary)',
        width: 52,
        flexShrink: 0,
        fontWeight: 600,
      }}>
        {p.identifier}
      </span>

      {/* Build progress bar — hidden when data is unavailable */}
      {pct !== null
        ? <ProgressBar pct={pct} style={{ flex: 1 }} />
        : <span style={{ flex: 1 }} />
      }

      {/* Build % — show "—" when total=0 (LIVE-03: never show 0%) */}
      <span style={{
        fontFamily: 'var(--font-mono)',
        fontSize: 'var(--text-xs)',
        color,
        width: 32,
        textAlign: 'right',
        flexShrink: 0,
        fontWeight: 600,
      }}>
        {pct !== null ? `${pct}%` : '—'}
      </span>

      {/* Enrichment separator */}
      <span style={{ color: 'var(--border-default)', fontSize: 'var(--text-xs)', flexShrink: 0 }}>·</span>

      {/* Enrichment indicator — ⚡ icon + % */}
      <span
        title={`Enriched: ${p.counts?.enriched ?? 0}/${p.counts?.enrichable ?? 0} tasks ready for build`}
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 3,
          flexShrink: 0,
          fontFamily: 'var(--font-mono)',
          fontSize: 'var(--text-xs)',
          color: eColor,
          fontWeight: 600,
          width: 48,
          justifyContent: 'flex-end',
        }}
      >
        <span style={{ fontSize: 9 }}>⚡</span>
        {ePct}%
      </span>
    </div>
  );
}
