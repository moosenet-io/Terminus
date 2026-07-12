// SGUI-09, ported for CONST-04: PRs stub page.
// The original read window.location.hostname to point at Gitea directly — dropped, since a
// same-origin control plane has no business telling the browser to hop to a different host/port
// (and per the hard rules, window.location may only ever be touched inside aggregationClient.ts).
export function PRs() {
  return (
    <div style={{ padding: 16, overflowY: 'auto', height: '100%' }}>
      <h2 style={{ fontSize: 16, fontWeight: 600, color: 'var(--h-teal)', marginBottom: 16 }}>Pull Requests</h2>
      <div className="h-card">
        <div className="h-card-body" style={{ color: 'var(--h-text-muted)', textAlign: 'center', padding: 24 }}>
          Recent PRs from Gitea will appear here once the Harmony aggregation endpoint is wired up.
        </div>
      </div>
    </div>
  );
}
