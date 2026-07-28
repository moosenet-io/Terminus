// LGUI-08 (§5 / §3.3): fixed tone mapping for `MemoryType` — violet=Principle, blue=Semantic,
// green=Preference, neutral=Episodic. NEVER derive the tone any other way; both this badge and
// the header legend (`MemoryTypeLegend` below) read the same `MEMORY_TYPE_TONE` map
// (`memorySearch.ts`) so they can never disagree.
import { Badge } from '../../components/Badge';
import { MEMORY_TYPE_TONE } from './memorySearch';
import type { MemoryType } from '../../types/luminaMemory';

export function MemoryTypeBadge({ type }: { type: MemoryType }) {
  return <Badge tone={MEMORY_TYPE_TONE[type]}>{type}</Badge>;
}

const LEGEND_ORDER: MemoryType[] = ['Principle', 'Semantic', 'Preference', 'Episodic'];

/** §3.3: "fixed mapping with a legend in the header." */
export function MemoryTypeLegend() {
  return (
    <div
      style={{
        display: 'flex', flexWrap: 'wrap', gap: 'var(--space-3)',
        fontSize: 'var(--fs-mono-sm)', color: 'var(--text-tertiary)',
      }}
    >
      {LEGEND_ORDER.map(t => (
        <span key={t} style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
          <MemoryTypeBadge type={t} />
        </span>
      ))}
    </div>
  );
}
