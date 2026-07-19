// SGUI-07: Start/Stop build + enrichment controls (Projects page)
import { useState } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';
import { RoleGate } from './RoleGate';

interface Props {
  engineState: string;
  isEnriching?: boolean;
  focusedProject?: string;
  projects: string[];
}

export function BuildControls({ engineState, isEnriching = false, focusedProject, projects }: Props) {
  const [selectedProject, setSelectedProject] = useState(focusedProject || projects[0] || '');
  const [buildLoading, setBuildLoading] = useState(false);
  const [enrichLoading, setEnrichLoading] = useState(false);

  const isBuilding = engineState !== 'STOPPED';
  const busy = buildLoading || enrichLoading;

  const sendCommand = async (cmd: string, setLoading: (v: boolean) => void) => {
    setLoading(true);
    try {
      await getAggregationClient().request('harmony', '/command', {
        method: 'POST',
        body: JSON.stringify({ command: cmd }),
      });
    } catch { /* ignore */ }
    setLoading(false);
  };

  const buildState = isBuilding ? engineState : 'IDLE';
  const buildColor = isBuilding ? 'var(--h-green)' : 'var(--h-text-dim)';
  const enrichColor = isEnriching ? 'var(--h-amber)' : 'var(--h-text-dim)';

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 8, padding: '8px 0' }}>
      {/* Project selector — shared by both build and enrich */}
      <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
        <select
          value={selectedProject}
          onChange={e => setSelectedProject(e.target.value)}
          disabled={busy}
          style={{
            background: 'var(--h-bg-card)', border: '1px solid var(--h-border)',
            borderRadius: 5, color: 'var(--h-text)', padding: '5px 10px', fontSize: 13, outline: 'none',
          }}
        >
          {projects.map(p => <option key={p} value={p}>{p}</option>)}
        </select>
      </div>

      {/* Build row — CONST-27: /command mutates, gated for a viewer session */}
      <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
        <span className="h-badge" style={{
          minWidth: 64, textAlign: 'center',
          background: isBuilding ? 'rgba(102,255,102,0.1)' : 'rgba(136,136,168,0.1)',
          color: buildColor,
        }}>
          {buildState}
        </span>
        <RoleGate>
          <button
            className="h-btn h-btn-green"
            disabled={busy || isBuilding || isEnriching || !selectedProject}
            onClick={() => sendCommand(`build ${selectedProject}`, setBuildLoading)}
          >
            {buildLoading ? '…' : `▶ Build`}
          </button>
        </RoleGate>
        <RoleGate>
          <button
            className="h-btn h-btn-red"
            disabled={busy || !isBuilding}
            onClick={() => sendCommand(':stop', setBuildLoading)}
          >
            {buildLoading ? '…' : '■ Stop'}
          </button>
        </RoleGate>
      </div>

      {/* Enrich row — CONST-27: /command mutates, gated for a viewer session */}
      <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
        <span className="h-badge" style={{
          minWidth: 64, textAlign: 'center',
          background: isEnriching ? 'rgba(255,200,50,0.1)' : 'rgba(136,136,168,0.1)',
          color: enrichColor,
        }}>
          {isEnriching ? 'ENRICHING' : 'IDLE'}
        </span>
        <RoleGate>
          <button
            className="h-btn h-btn-teal"
            disabled={busy || isEnriching || isBuilding || !selectedProject}
            onClick={() => sendCommand(`enrich ${selectedProject}`, setEnrichLoading)}
            title="Enriches task descriptions using the current inference notch setting"
          >
            {enrichLoading ? '…' : `⚡ Enrich`}
          </button>
        </RoleGate>
        <RoleGate>
          <button
            className="h-btn h-btn-red"
            disabled={busy || !isEnriching}
            onClick={() => sendCommand('stop-enrich', setEnrichLoading)}
          >
            {enrichLoading ? '…' : '■ Stop'}
          </button>
        </RoleGate>
      </div>
    </div>
  );
}
