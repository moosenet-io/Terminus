// CONST-04: Lumina has no config surface yet (tracked for CONST-07). This stub keeps the
// registry/nav grouping complete without pretending a backend exists — `available: false` in
// registerPanels.ts is what actually keeps it out of the nav; this component is dead code until
// that flips, kept only so the wiring is a one-line change when CONST-07 lands.
import { Card, CardTitle } from '../../components/Card';

export function LuminaStubPanel() {
  return (
    <div style={{ padding: 'var(--space-5)' }}>
      <CardTitle subtitle="Lumina's own config surface lands in CONST-07">Lumina</CardTitle>
      <Card variant="content">
        <span style={{ color: 'var(--text-tertiary)' }}>Not yet available.</span>
      </Card>
    </div>
  );
}
