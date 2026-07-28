// LGUI-08 (§3.3): sensitivity Badge — Health/Finance/Personal ALWAYS carry a lock glyph
// (`is_always_private`), independent of the record's actual `visibility`. Other categories get
// a plain neutral badge with no lock. `None` renders as a muted dash-style badge so an
// unclassified record doesn't read as falsely reassuring ("this one's fine") vs. simply absent.
import { Badge } from '../../components/Badge';
import { isAlwaysPrivate } from '../../types/luminaMemory';
import type { SensitivityCategory } from '../../types/luminaMemory';

const LOCK_GLYPH = '\u{1F512}'; // 🔒

export function SensitivityBadge({ sensitivity }: { sensitivity: SensitivityCategory }) {
  const alwaysPrivate = isAlwaysPrivate(sensitivity);
  if (sensitivity === 'None') {
    return <Badge tone="neutral">none</Badge>;
  }
  return (
    <Badge tone={alwaysPrivate ? 'rose' : 'amber'}>
      {alwaysPrivate && (
        <span aria-hidden style={{ marginRight: 4 }}>{LOCK_GLYPH}</span>
      )}
      <span className="sr-only" style={{ position: 'absolute', width: 1, height: 1, overflow: 'hidden', clip: 'rect(0 0 0 0)' }}>
        {alwaysPrivate ? 'always private: ' : ''}
      </span>
      {sensitivity}
    </Badge>
  );
}
