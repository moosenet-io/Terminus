// SGUI-07: Projects page with Start/Stop controls
import { useState, useEffect, useCallback } from 'react';
import type { Project } from '../types/api';
import { BuildControls } from '../components/BuildControls';
import { getAggregationClient } from '../lib/aggregationClient';

interface Props {
  engineState: string;
  isEnriching?: boolean;
  /** Live project list from App.tsx WS subscription — updated every ~30s via tui-state events */
  liveProjects?: Project[];
}

export function Projects({ engineState, isEnriching = false, liveProjects }: Props) {
  const [projects, setProjects] = useState<Project[]>(liveProjects ?? []);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [loading, setLoading] = useState(!liveProjects?.length);
  const [lastFetch, setLastFetch] = useState(0);

  const fetchProjects = useCallback(() => {
    getAggregationClient()
      .request<{ projects?: Project[] }>('harmony', '/status')
      .then(d => {
        setProjects(d.projects || []);
        setLoading(false);
        setLastFetch(Date.now());
      })
      .catch(() => setLoading(false));
  }, []);

  // Initial load
  useEffect(() => { fetchProjects(); }, [fetchProjects]);

  // Refresh every 30 seconds while the page is open — Plane data in the state-cache
  // daemon polls at 60s intervals, and the REST /api/status always has the latest.
  useEffect(() => {
    const id = setInterval(fetchProjects, 30_000);
    return () => clearInterval(id);
  }, [fetchProjects]);

  // Sync immediately whenever App.tsx delivers fresh WS-driven data
  useEffect(() => {
    if (liveProjects?.length) {
      setProjects(liveProjects);
      setLoading(false);
    }
  }, [liveProjects]);

  const projectIds = projects.map(p => p.identifier);

  const ageSeconds = lastFetch ? Math.round((Date.now() - lastFetch) / 1000) : null;

  return (
    <div style={{ padding: 16, overflowY: 'auto', height: '100%' }}>
      <div style={{ display: 'flex', alignItems: 'baseline', justifyContent: 'space-between', marginBottom: 16 }}>
        <h2 style={{ fontSize: 16, fontWeight: 600, color: 'var(--accent-primary)' }}>Projects</h2>
        {ageSeconds !== null && (
          <span style={{ fontSize: 'var(--text-xs)', color: 'var(--text-tertiary)', fontFamily: 'var(--font-mono)' }}>
            updated {ageSeconds < 5 ? 'just now' : `${ageSeconds}s ago`}
          </span>
        )}
      </div>
      <BuildControls engineState={engineState} isEnriching={isEnriching} projects={projectIds} />
      <div style={{ marginTop: 16 }}>
        {loading ? <div className="h-skeleton" style={{ height: 60 }} /> : projects.map(p => (
          <ProjectRow key={p.identifier} project={p} expanded={expanded === p.identifier} onToggle={() => setExpanded(e => e === p.identifier ? null : p.identifier)} />
        ))}
      </div>
    </div>
  );
}

function ProjectRow({ project: p, expanded, onToggle }: { project: Project; expanded: boolean; onToggle: () => void }) {
  const total = (p.counts?.todo||0) + (p.counts?.in_progress||0) + (p.counts?.done||0);
  const pct = p.progress_pct ?? (total > 0 ? Math.round(p.counts.done * 100 / total) : 0);
  const ePct = p.enrichment_pct ?? 0;
  const buildColor = pct > 80 ? 'var(--status-success)' : pct > 20 ? 'var(--status-warning)' : 'var(--status-error)';
  // Enrichment color: red < 30%, amber 30-60%, teal 60-99%, green 100%
  const enrichColor = ePct >= 100 ? 'var(--status-success)' : ePct >= 60 ? 'var(--accent-primary)' : ePct >= 30 ? 'var(--status-warning)' : 'var(--status-error)';

  return (
    <div className="h-card" style={{ marginBottom: 8 }}>
      <div className="h-card-header" onClick={onToggle}>
        <div style={{ flex: 1, minWidth: 0 }}>
          {/* Top row: identifier + name */}
          <div className="h-flex h-gap-md" style={{ marginBottom: 6 }}>
            <span className="h-mono" style={{ color: 'var(--accent-primary)', fontSize: 12, width: 60, flexShrink: 0, fontWeight: 600 }}>
              {p.identifier}
            </span>
            <span style={{ fontSize: 13, color: 'var(--text-primary)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
              {p.name}
            </span>
          </div>

          {/* Build progress row */}
          <div className="h-flex h-gap-md" style={{ alignItems: 'center' }}>
            <span style={{ fontSize: 10, color: 'var(--text-tertiary)', width: 60, flexShrink: 0 }}>Build</span>
            <div className="h-progress" style={{ flex: 1, height: 5 }}>
              <div className="h-progress-fill" style={{ width: `${pct}%`, background: buildColor }} />
            </div>
            <span className="h-mono" style={{ fontSize: 12, color: buildColor, width: 38, textAlign: 'right', flexShrink: 0, fontWeight: 600 }}>
              {pct}%
            </span>
            <span className="h-mono" style={{ fontSize: 11, color: 'var(--text-tertiary)', width: 60, textAlign: 'right', flexShrink: 0 }}>
              {p.counts?.done ?? 0}/{total}
            </span>
          </div>

          {/* Enrichment progress row — per behavior contract §1.2: enrichment readiness feeds into execution */}
          <div className="h-flex h-gap-md" style={{ alignItems: 'center', marginTop: 4 }}>
            <span style={{ fontSize: 10, color: 'var(--text-tertiary)', width: 60, flexShrink: 0 }}>⚡ Enrich</span>
            <div className="h-progress" style={{ flex: 1, height: 3 }}>
              <div className="h-progress-fill" style={{
                width: `${ePct}%`,
                background: enrichColor,
                opacity: 0.7,
              }} />
            </div>
            <span className="h-mono" style={{ fontSize: 12, color: enrichColor, width: 38, textAlign: 'right', flexShrink: 0, fontWeight: 600 }}>
              {ePct}%
            </span>
            <span className="h-mono" style={{ fontSize: 11, color: 'var(--text-tertiary)', width: 60, textAlign: 'right', flexShrink: 0 }}>
              {p.counts?.enriched ?? 0}/{p.counts?.enrichable ?? 0}
            </span>
          </div>
        </div>

        <span style={{ color: 'var(--text-tertiary)', fontSize: 12, transform: expanded ? 'rotate(180deg)' : 'none', transition: 'transform 0.2s', marginLeft: 8, flexShrink: 0 }}>▼</span>
      </div>

      {expanded && (
        <div className="h-card-body">
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(5, 1fr)', gap: 8, textAlign: 'center' }}>
            {([
              ['Backlog', p.counts?.todo??0, 'var(--text-secondary)'],
              ['In Progress', p.counts?.in_progress??0, 'var(--status-info)'],
              ['Done', p.counts?.done??0, 'var(--status-success)'],
              ['Enriched', p.counts?.enriched??0, 'var(--accent-primary)'],
              ['Total', total, 'var(--text-primary)'],
            ] as [string, number, string][]).map(([label, count, color]) => (
              <div key={label} className="h-card" style={{ padding: '8px 0' }}>
                <div style={{ fontSize: 18, fontWeight: 700, color }}>{count}</div>
                <div style={{ fontSize: 11, color: 'var(--text-tertiary)', marginTop: 2 }}>{label}</div>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
