// TRCI-02: Prompt version registry viewer with side-by-side diff.
import { useState, useEffect } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';

interface PromptVersion {
  id: string;
  step_type: string;
  version: number;
  template_hash: string;
  template_text: string;
  created_at: string;
  trigger: string;
}

function diffLines(a: string, b: string): { type: 'same' | 'add' | 'remove'; text: string }[] {
  const aLines = a.split('\n');
  const bLines = b.split('\n');
  const result: { type: 'same' | 'add' | 'remove'; text: string }[] = [];
  const maxLen = Math.max(aLines.length, bLines.length);
  for (let i = 0; i < maxLen; i++) {
    const al = aLines[i] ?? '';
    const bl = bLines[i] ?? '';
    if (al === bl) {
      result.push({ type: 'same', text: al });
    } else {
      if (aLines[i] !== undefined) result.push({ type: 'remove', text: al });
      if (bLines[i] !== undefined) result.push({ type: 'add', text: bl });
    }
  }
  return result;
}

export function Prompts() {
  const [versions, setVersions] = useState<PromptVersion[]>([]);
  const [loading, setLoading] = useState(true);
  const [selected, setSelected] = useState<string[]>([]);
  const [stepFilter, setStepFilter] = useState('');

  useEffect(() => {
    const params = stepFilter ? `?step_type=${encodeURIComponent(stepFilter)}` : '';
    getAggregationClient()
      .request<{ versions?: PromptVersion[] }>('harmony', `/prompts${params}`)
      .then(d => { setVersions(d.versions || []); setLoading(false); })
      .catch(() => setLoading(false));
  }, [stepFilter]);

  const steps = [...new Set(versions.map(v => v.step_type))].sort();
  const filtered = stepFilter ? versions.filter(v => v.step_type === stepFilter) : versions;
  const selA = versions.find(v => v.id === selected[0]);
  const selB = versions.find(v => v.id === selected[1]);

  const toggleSelect = (id: string) => {
    setSelected(prev => {
      if (prev.includes(id)) return prev.filter(x => x !== id);
      if (prev.length < 2) return [...prev, id];
      return [prev[1], id]; // replace oldest selection
    });
  };

  const diff = selA && selB ? diffLines(selA.template_text, selB.template_text) : null;

  return (
    <div style={{ padding: 16, overflowY: 'auto', height: '100%', display: 'flex', flexDirection: 'column', gap: 12 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
        <h2 style={{ fontSize: 16, fontWeight: 600, color: 'var(--h-teal)', margin: 0 }}>Prompt Versions</h2>
        <select value={stepFilter} onChange={e => setStepFilter(e.target.value)}
          style={{ background: 'var(--h-bg-card)', border: '1px solid var(--h-border)', borderRadius: 4, color: 'var(--h-text)', padding: '3px 8px', fontSize: 12, outline: 'none' }}>
          <option value="">All steps</option>
          {steps.map(s => <option key={s} value={s}>{s}</option>)}
        </select>
        {selected.length > 0 && (
          <span style={{ fontSize: 11, color: 'var(--h-text-muted)' }}>
            {selected.length === 2 ? 'Showing diff below' : `Select 1 more to diff`}
          </span>
        )}
      </div>

      {loading ? <div className="h-skeleton" style={{ height: 100 }} /> : filtered.length === 0 ? (
        <div style={{ color: 'var(--h-text-muted)', fontSize: 13 }}>No prompt versions recorded yet. They appear here as tasks execute.</div>
      ) : (
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(280px, 1fr))', gap: 8 }}>
          {filtered.map(v => {
            const isSel = selected.includes(v.id);
            return (
              <div key={v.id} onClick={() => toggleSelect(v.id)} className="h-card"
                style={{ cursor: 'pointer', border: `1px solid ${isSel ? 'var(--h-teal)' : 'var(--h-border)'}`, background: isSel ? 'var(--h-bg-active)' : undefined }}>
                <div className="h-card-body">
                  <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: 4 }}>
                    <span style={{ fontSize: 11, color: 'var(--h-teal)', fontWeight: 600 }}>{v.step_type} v{v.version}</span>
                    <span style={{ fontSize: 10, color: 'var(--h-text-muted)' }}>{v.created_at.slice(0, 10)}</span>
                  </div>
                  <div style={{ fontSize: 10, color: 'var(--h-text-muted)', marginBottom: 4 }} className="h-mono">{v.template_hash.slice(0, 12)}…</div>
                  {v.trigger && <div style={{ fontSize: 10, color: 'var(--h-text-dim)' }}>{v.trigger}</div>}
                  <div style={{ fontSize: 10, color: 'var(--h-text-muted)', marginTop: 4, maxHeight: 40, overflow: 'hidden' }}>
                    {v.template_text.slice(0, 80)}…
                  </div>
                </div>
              </div>
            );
          })}
        </div>
      )}

      {/* Diff view */}
      {diff && selA && selB && (
        <div className="h-card">
          <div className="h-card-header" style={{ cursor: 'default' }}>
            <span style={{ fontWeight: 600, fontSize: 13 }}>Diff: {selA.step_type} v{selA.version} → v{selB.version}</span>
          </div>
          <div className="h-card-body" style={{ fontFamily: 'var(--h-font-mono)', fontSize: 11 }}>
            {diff.slice(0, 200).map((line, i) => (
              <div key={i} style={{
                background: line.type === 'add' ? 'rgba(102,255,102,0.08)' : line.type === 'remove' ? 'rgba(255,68,68,0.08)' : undefined,
                color: line.type === 'add' ? 'var(--h-green)' : line.type === 'remove' ? 'var(--h-red)' : 'var(--h-text-dim)',
                padding: '1px 6px',
                whiteSpace: 'pre-wrap',
                wordBreak: 'break-all',
              }}>
                {line.type === 'add' ? '+ ' : line.type === 'remove' ? '- ' : '  '}{line.text}
              </div>
            ))}
            {diff.length > 200 && <div style={{ color: 'var(--h-text-muted)', fontSize: 10, padding: '4px 6px' }}>… {diff.length - 200} more lines</div>}
          </div>
        </div>
      )}
    </div>
  );
}
