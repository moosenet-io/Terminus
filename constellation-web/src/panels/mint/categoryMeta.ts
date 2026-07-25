// CGUI-10 (TERM #533): MINT category taxonomy + metric helpers. Pure, side-effect-free
// (logic-only unit-tested — same convention as the other *.test.ts suites; no jsdom).
//
// The MINT module reports on every benchmark category the fleet profiles. Two lineages share
// one uniform report template (see CategoryReportPanel):
//   - `newcat`  : the 8 new task-categories, each with its own per-category read endpoints
//                 (`client.mint.category*`, CGUI-07/08). Driven by a category `clientKey`.
//   - `legacy`  : the 3 original suites (code / context / agent) — no per-category endpoints;
//                 they read the fleet-wide MINT views (`dimensions`/`matrix`/`box`/`runs`/
//                 `failures`) filtered by suite where the endpoint supports it.
//   - `persona` : the assistant capability-radar view over `client.mint.dimensions()` — the
//                 8 `ASSISTANT_DIMENSIONS` (conversation_depth, personality_*, …) per model.
//
// Every value a report shows is a live DB read through the CGUI-08 data client; nothing here
// hardcodes a score. `metricUnitScore` is the ONE place metric-scale knowledge lives so the
// radar axis, heatmap color, and ranking bar all agree on "higher = better capability".
import type { MintCategoryKey } from '../../lib/aggregationClient';

export type CategoryKind = 'newcat' | 'legacy' | 'persona';

export interface CategoryMeta {
  /** Tab id / URL slug (`/mint/category?c=<id>`). */
  id: string;
  /** Rail/tab label. */
  label: string;
  /** One-line description shown under the report header. */
  blurb: string;
  kind: CategoryKind;
  /** For `newcat`: the canonical/alias key handed to `client.mint.category*`. */
  clientKey?: MintCategoryKey;
  /** For `legacy`: the suite value handed to `client.mint.runs({ suite })`. */
  legacySuite?: 'code' | 'context' | 'agent';
  /** Coarse grouping for the category picker (visual sectioning only). */
  group: 'Retrieval' | 'Multimodal' | 'Agentic' | 'Code' | 'Assistant';
}

/** All MINT categories, in display order: the 8 new task-categories first (the primary
 *  deliverable), then the 3 legacy suites, then the persona/assistant radar. */
export const MINT_CATEGORY_META: readonly CategoryMeta[] = [
  { id: 'embedding_retrieval', label: 'Embedding Retrieval', blurb: 'Dense-retrieval quality — nDCG / MRR / recall across the embedding fleet.', kind: 'newcat', clientKey: 'embedding_retrieval', group: 'Retrieval' },
  { id: 'reranking', label: 'Reranking', blurb: 'Cross-encoder rerank quality — nDCG / MAP lift over first-stage retrieval.', kind: 'newcat', clientKey: 'reranking', group: 'Retrieval' },
  { id: 'image_parsing', label: 'Vision QA', blurb: 'Image understanding — answer accuracy and description quality (vision_qa).', kind: 'newcat', clientKey: 'image_parsing', group: 'Multimodal' },
  { id: 'document_parsing', label: 'Document Parsing', blurb: 'OCR + layout extraction — character error rate and layout F1.', kind: 'newcat', clientKey: 'document_parsing', group: 'Multimodal' },
  { id: 'image_generation', label: 'Image Generation', blurb: 'Text-to-image fidelity — CLIP alignment and aesthetic score.', kind: 'newcat', clientKey: 'image_generation', group: 'Multimodal' },
  { id: 'voice_transcription', label: 'Speech-to-Text', blurb: 'ASR transcription — word / character error rate (stt).', kind: 'newcat', clientKey: 'voice_transcription', group: 'Multimodal' },
  { id: 'tts', label: 'Text-to-Speech', blurb: 'Speech synthesis — mean opinion score and round-trip WER.', kind: 'newcat', clientKey: 'tts', group: 'Multimodal' },
  { id: 'tool_routing', label: 'Tool Routing', blurb: 'Tool-selection accuracy — routing accuracy and F1 over the tool set.', kind: 'newcat', clientKey: 'tool_routing', group: 'Agentic' },
  { id: 'code', label: 'Code (legacy)', blurb: 'Original coder suite — capability radar, coverage matrix, timing distribution.', kind: 'legacy', legacySuite: 'code', group: 'Code' },
  { id: 'context', label: 'Context (legacy)', blurb: 'Long-context suite — recall + throughput across context tiers.', kind: 'legacy', legacySuite: 'context', group: 'Code' },
  { id: 'agent', label: 'Agent (legacy)', blurb: 'Agentic suite — multi-step tool-use and task completion runs.', kind: 'legacy', legacySuite: 'agent', group: 'Agentic' },
  { id: 'persona', label: 'Persona / Assistant', blurb: 'Assistant capability radar — the 8 ASSISTANT_DIMENSIONS per model.', kind: 'persona', group: 'Assistant' },
] as const;

export function categoryById(id: string): CategoryMeta | undefined {
  return MINT_CATEGORY_META.find(c => c.id === id);
}

export const DEFAULT_CATEGORY_ID = MINT_CATEGORY_META[0].id;

// ── Metric-scale knowledge ────────────────────────────────────────────────────

/** Metrics where a LOWER raw value is better (error rates, latencies). Everything else is
 *  treated as higher-is-better on a 0..1 (or otherwise-scaled, see metricUnitScore) axis. */
export const LOWER_IS_BETTER = new Set<string>([
  'wer', 'cer', 'total_time_ms', 'mean_latency_ms', 'p95_latency_ms', 'latency_ms', 'ttft_ms',
]);

function clamp01(v: number): number {
  if (!Number.isFinite(v)) return 0;
  return v < 0 ? 0 : v > 1 ? 1 : v;
}

/**
 * Map a raw metric value to a unit capability score in [0,1] where 1 = best. This is the
 * single source of scale-truth shared by the radar axis, the heatmap color ramp, and the
 * ranking bar so all three read consistently regardless of the metric's native units.
 *   - mos            → /5   (opinion score 0–5)
 *   - aesthetic_score→ /10  (0–10 scale)
 *   - *_ms / latency → soft 1/(1+v/2000) decay (lower = better, unbounded units)
 *   - wer / cer      → 1 − v (0–1 error, lower = better)
 *   - everything else→ v clamped (already a 0–1 higher-is-better score)
 */
export function metricUnitScore(metric: string, value: number | null | undefined): number {
  if (value == null || !Number.isFinite(value)) return 0;
  const m = metric.toLowerCase();
  if (m === 'mos') return clamp01(value / 5);
  if (m === 'aesthetic_score') return clamp01(value / 10);
  if (LOWER_IS_BETTER.has(m)) {
    if (m.endsWith('_ms') || m.includes('latency') || m.includes('time')) {
      return clamp01(1 / (1 + value / 2000));
    }
    return clamp01(1 - value); // 0–1 error rate
  }
  return clamp01(value);
}

/** Human label for a metric/dimension id (`ndcg_at_10` → `nDCG@10`, `mean_latency_ms` →
 *  `Mean Latency Ms`). Keeps a few well-known acronyms uppercase. */
export function metricLabel(metric: string): string {
  const special: Record<string, string> = {
    ndcg_at_10: 'nDCG@10', mrr: 'MRR', recall_at_10: 'Recall@10', map: 'MAP', f1: 'F1',
    cer: 'CER', wer: 'WER', mos: 'MOS', clip_score: 'CLIP Score', layout_f1: 'Layout F1',
    total_time_ms: 'Total Time (ms)', mean_latency_ms: 'Mean Latency (ms)',
  };
  if (special[metric]) return special[metric];
  return metric
    .split('_')
    .map(w => (w.length ? w[0].toUpperCase() + w.slice(1) : w))
    .join(' ');
}

/** Format a raw metric value for display: ms as integers with a unit, everything else to 3
 *  significant decimals (trimmed). Null renders as an em dash. */
export function formatMetricValue(metric: string, value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return '—';
  const m = metric.toLowerCase();
  if (m.endsWith('_ms') || m.includes('latency') || m.includes('time')) {
    return `${Math.round(value).toLocaleString()} ms`;
  }
  const r = Math.round(value * 1000) / 1000;
  return String(r);
}
