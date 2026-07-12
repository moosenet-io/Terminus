// TRCI-05: Prompt playground — live prompt testing against any model.
import { useState, useCallback } from 'react';
import { getAggregationClient } from '../lib/aggregationClient';

interface PlaygroundResponse {
  response: string;
  tokens_in: number;
  tokens_out: number;
  latency_ms: number;
  cost: number;
  model: string;
}

const MODELS = [
  { value: 'qwen3:8b',        label: 'qwen3:8b (Ollama CPU · free)' },
  { value: 'qwen3-coder-30b', label: 'qwen3-coder-30b (Ollama GPU · free)' },
  { value: 'claude/sonnet',   label: 'claude / sonnet (cloud)' },
  { value: 'claude/haiku',    label: 'claude / haiku (cloud · cheap)' },
  { value: 'gpt-5.5',         label: 'codex / gpt-5.5 (cloud)' },
  { value: 'gemini/2.5-pro',  label: 'gemini / 2.5 pro (cloud)' },
];

export function Playground() {
  const [prompt, setPrompt] = useState('');
  const [model, setModel] = useState('qwen3:8b');
  const [temperature, setTemperature] = useState(0.6);
  const [topP, setTopP] = useState(0.9);
  const [maxTokens, setMaxTokens] = useState(500);
  const [result, setResult] = useState<PlaygroundResponse | null>(null);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);

  const run = useCallback(async () => {
    if (!prompt.trim()) return;
    setLoading(true);
    setError('');
    setResult(null);
    try {
      const data = await getAggregationClient().request<PlaygroundResponse>('chord', '/playground/run', {
        method: 'POST',
        body: JSON.stringify({ prompt: prompt.trim(), model, temperature, top_p: topP, max_tokens: maxTokens }),
      });
      setResult(data);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
    setLoading(false);
  }, [prompt, model, temperature, topP, maxTokens]);

  return (
    <div style={{ padding: 16, height: '100%', display: 'flex', flexDirection: 'column', gap: 12, overflowY: 'auto' }}>
      <h2 style={{ fontSize: 16, fontWeight: 600, color: 'var(--h-teal)', margin: 0 }}>Prompt Playground</h2>

      {/* Settings bar */}
      <div className="h-card">
        <div className="h-card-body" style={{ display: 'flex', flexWrap: 'wrap', gap: 12, alignItems: 'center' }}>
          <div>
            <div style={{ fontSize: 10, color: 'var(--h-text-muted)', marginBottom: 3 }}>Model</div>
            <select value={model} onChange={e => setModel(e.target.value)}
              style={{ background: 'var(--h-bg-card)', border: '1px solid var(--h-border)', borderRadius: 4, color: 'var(--h-text)', padding: '4px 8px', fontSize: 12, outline: 'none' }}>
              {MODELS.map(m => <option key={m.value} value={m.value}>{m.label}</option>)}
            </select>
          </div>
          {[
            { label: 'Temperature', value: temperature, min: 0, max: 2, step: 0.05, setter: setTemperature },
            { label: 'Top-p', value: topP, min: 0, max: 1, step: 0.05, setter: setTopP },
          ].map(({ label, value, min, max, step, setter }) => (
            <div key={label}>
              <div style={{ fontSize: 10, color: 'var(--h-text-muted)', marginBottom: 3 }}>{label}: {value.toFixed(2)}</div>
              <input type="range" min={min} max={max} step={step} value={value}
                onChange={e => setter(parseFloat(e.target.value))}
                style={{ width: 100 }} />
            </div>
          ))}
          <div>
            <div style={{ fontSize: 10, color: 'var(--h-text-muted)', marginBottom: 3 }}>Max tokens</div>
            <input type="number" value={maxTokens} min={10} max={4000}
              onChange={e => setMaxTokens(parseInt(e.target.value) || 500)}
              style={{ width: 70, background: 'var(--h-bg-card)', border: '1px solid var(--h-border)', borderRadius: 4, color: 'var(--h-text)', padding: '4px 6px', fontSize: 12, outline: 'none' }} />
          </div>
        </div>
      </div>

      {/* Editor + response */}
      <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12, flex: 1, minHeight: 0 }}>
        <div className="h-card" style={{ display: 'flex', flexDirection: 'column' }}>
          <div className="h-card-header" style={{ cursor: 'default' }}>
            <span style={{ fontWeight: 600, fontSize: 13 }}>Prompt</span>
            <button className="h-btn h-btn-teal" onClick={run} disabled={loading || !prompt.trim()}
              style={{ padding: '4px 14px', fontSize: 12 }}>
              {loading ? '…' : '▶ Run'}
            </button>
          </div>
          <div className="h-card-body" style={{ flex: 1, display: 'flex' }}>
            <textarea value={prompt} onChange={e => setPrompt(e.target.value)}
              onKeyDown={e => { if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) run(); }}
              placeholder="Enter your prompt… (Cmd+Enter to run)"
              style={{
                flex: 1, width: '100%', resize: 'none', border: 'none', outline: 'none',
                background: 'transparent', color: 'var(--h-text)', fontSize: 12,
                fontFamily: 'var(--h-font-mono)', lineHeight: 1.5,
              }} />
          </div>
        </div>

        <div className="h-card" style={{ display: 'flex', flexDirection: 'column' }}>
          <div className="h-card-header" style={{ cursor: 'default' }}>
            <span style={{ fontWeight: 600, fontSize: 13 }}>Response</span>
            {result && (
              <span style={{ fontSize: 11, color: 'var(--h-text-muted)' }}>
                {result.tokens_in}→{result.tokens_out} tokens · {result.latency_ms}ms · ${result.cost.toFixed(4)}
              </span>
            )}
          </div>
          <div className="h-card-body" style={{ flex: 1, overflowY: 'auto' }}>
            {loading && <div style={{ color: 'var(--h-text-muted)', fontSize: 12 }}>Thinking…</div>}
            {error && <div style={{ color: 'var(--h-red)', fontSize: 12 }}>{error}</div>}
            {result && (
              <pre style={{ fontSize: 12, color: 'var(--h-text)', whiteSpace: 'pre-wrap', wordBreak: 'break-word', margin: 0 }}>
                {result.response}
              </pre>
            )}
            {!loading && !error && !result && (
              <div style={{ color: 'var(--h-text-muted)', fontSize: 12 }}>Response will appear here.</div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
