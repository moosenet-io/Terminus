// SGUI-09: Tasks stub page
export function Tasks() {
  return (
    <div style={{ padding: 16, overflowY: 'auto', height: '100%' }}>
      <h2 style={{ fontSize: 16, fontWeight: 600, color: 'var(--h-teal)', marginBottom: 16 }}>Tasks</h2>
      <div className="h-card">
        <div className="h-card-body" style={{ color: 'var(--h-text-muted)', textAlign: 'center', padding: 24 }}>
          Task data available via Plane API. Use the Projects page to view per-project task breakdowns.
        </div>
      </div>
    </div>
  );
}
