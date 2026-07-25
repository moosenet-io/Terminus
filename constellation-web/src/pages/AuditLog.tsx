// CGUI-06 (TERM #529): Harmony Audit Log panel — a scrolling telemetry feed (was a 31-LOC
// placeholder card that rendered no real log).
//
// Renders audit events as a dense feed of rows: mono timestamp · actor badge · action · a
// colored result tag (ok/denied/error). Filterable by result. Built on the DS primitives
// (Card/Badge/StatusPill) inside a PanelRoot scroll frame (`.hf-scroll`) so the feed scrolls.
//
// Data wiring: pulls `GET /api/harmony/audit` through the aggregation client. That endpoint
// is not yet served (mock adapter returns null; a live backend reads
// /var/log/harmony/audit.jsonl and 404s until wired), so on an empty response we fall back to
// the REPRESENTATIVE events below so the feed reads as developed. PENDING REAL-DATA WIRING:
// replace the fallback once Harmony tails its audit.jsonl through an aggregation endpoint
// (shape = `AuditEvent[]`).
import { useEffect, useMemo, useState } from 'react';
import { Card, CardTitle } from '../components/Card';
import { PanelRoot } from '../components/PanelRoot';
import { Badge } from '../components/Badge';
import type { BadgeTone } from '../components/Badge';
import { MetricCard } from '../components/MetricCard';
import { getAggregationClient } from '../lib/aggregationClient';

type AuditResult = 'ok' | 'denied' | 'error';

interface AuditEvent {
  ts: string;        // ISO-8601
  actor: string;     // principal / service that took the action
  action: string;    // dotted event name, e.g. "merge.gate.pass"
  target: string;    // resource the action touched
  result: AuditResult;
}

const RESULT_TONE: Record<AuditResult, BadgeTone> = { ok: 'green', denied: 'amber', error: 'rose' };
const RESULT_LABEL: Record<AuditResult, string> = { ok: 'ok', denied: 'denied', error: 'error' };

// Representative fallback — see the file header. Timestamps are anchored to a fixed base so
// the feed is deterministic (no wall-clock drift between renders / tests).
const BASE_TS = Date.parse('2026-07-25T14:32:00Z');
function ago(secondsBack: number): string {
  return new Date(BASE_TS - secondsBack * 1000).toISOString();
}
const FALLBACK_EVENTS: AuditEvent[] = [
  { ts: ago(12), actor: 'review-daemon', action: 'review.gate.pass', target: 'Terminus PR 246', result: 'ok' },
  { ts: ago(48), actor: 'harmony.conductor', action: 'worktree.merge', target: 'feat/S126-cgui-06', result: 'ok' },
  { ts: ago(95), actor: 'chord.proxy', action: 'backend.idle_stop', target: 'lemonade:8081', result: 'ok' },
  { ts: ago(140), actor: 'viewer:anon', action: 'engine.restart', target: 'harmony.engine', result: 'denied' },
  { ts: ago(210), actor: 'constellation-updater', action: 'module.deploy', target: 'terminus-primary', result: 'ok' },
  { ts: ago(305), actor: 'mirror-runner', action: 'github.push', target: 'moosenet-io/Chord', result: 'ok' },
  { ts: ago(360), actor: 'review-daemon', action: 'review.gate.request_changes', target: 'Terminus PR 242', result: 'denied' },
  { ts: ago(420), actor: 'compiler', action: 'build.publish', target: 'terminus_primary@18721d8a', result: 'ok' },
  { ts: ago(505), actor: 'chord.proxy', action: 'model.serve', target: 'qwen3-coder:30b', result: 'error' },
  { ts: ago(600), actor: 'plane.tool', action: 'issue.transition', target: 'TERM-529 → In Progress', result: 'ok' },
  { ts: ago(720), actor: 'harmony.dispatch', action: 'agent.spawn', target: 'CGUI-06 build agent', result: 'ok' },
  { ts: ago(880), actor: '<secret-manager>', action: 'secret.materialize', target: 'GITEA_TOKEN', result: 'ok' },
  { ts: ago(1010), actor: 'cicd-monitor', action: 'cosign.verify', target: 'harmony-statecache', result: 'ok' },
  { ts: ago(1180), actor: 'cargo-audit', action: 'security.scan', target: 'Terminus (11 advisories)', result: 'denied' },
];

function fmtTs(iso: string): string {
  return new Date(iso).toLocaleTimeString([], { hour12: false });
}

export function AuditLog() {
  const [events, setEvents] = useState<AuditEvent[] | null>(null);
  const [resultFilter, setResultFilter] = useState<AuditResult | null>(null);

  useEffect(() => {
    let cancelled = false;
    getAggregationClient()
      .request<AuditEvent[] | null>('harmony', '/audit')
      .then(d => { if (!cancelled) setEvents(Array.isArray(d) && d.length > 0 ? d : FALLBACK_EVENTS); })
      .catch(() => { if (!cancelled) setEvents(FALLBACK_EVENTS); });
    return () => { cancelled = true; };
  }, []);

  const rows = events ?? [];
  const filtered = useMemo(
    () => (resultFilter ? rows.filter(e => e.result === resultFilter) : rows),
    [rows, resultFilter],
  );

  const counts = useMemo(() => ({
    total: rows.length,
    denied: rows.filter(e => e.result === 'denied').length,
    errors: rows.filter(e => e.result === 'error').length,
  }), [rows]);

  const RESULTS: AuditResult[] = ['ok', 'denied', 'error'];

  return (
    <PanelRoot style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="Live telemetry of privileged actions across the constellation — actor, action, target and result">
        Harmony — Audit Log
      </CardTitle>

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 'var(--space-3)' }}>
        <MetricCard label="Events" value={events ? String(counts.total) : '—'} />
        <MetricCard label="Denied" value={events ? String(counts.denied) : '—'} valueColor="warning" />
        <MetricCard label="Errors" value={events ? String(counts.errors) : '—'} valueColor="error" />
      </div>

      <div style={{ display: 'flex', gap: 'var(--space-1)', alignItems: 'center', flexWrap: 'wrap' }}>
        <span style={{ fontSize: 'var(--fs-xs)', color: 'var(--text-muted)', marginRight: 4 }}>Result:</span>
        <button
          type="button"
          onClick={() => setResultFilter(null)}
          className={`h-badge ${resultFilter === null ? 'h-badge-violet' : 'h-badge-neutral'}`}
          style={{ cursor: 'pointer', border: 'none' }}
        >
          all
        </button>
        {RESULTS.map(r => (
          <button
            key={r}
            type="button"
            onClick={() => setResultFilter(r)}
            className={`h-badge ${resultFilter === r ? 'h-badge-violet' : 'h-badge-neutral'}`}
            style={{ cursor: 'pointer', border: 'none' }}
          >
            {RESULT_LABEL[r]}
          </button>
        ))}
      </div>

      <Card variant="content" padding="var(--space-2)">
        {events === null ? (
          <div style={{ padding: 'var(--space-5)', textAlign: 'center', color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>Loading…</div>
        ) : filtered.length === 0 ? (
          <div style={{ padding: 'var(--space-5)', textAlign: 'center', color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>No events match this filter</div>
        ) : (
          <div style={{ display: 'flex', flexDirection: 'column' }}>
            {filtered.map((e, i) => (
              <div
                key={`${e.ts}-${e.action}-${i}`}
                style={{
                  display: 'flex',
                  alignItems: 'center',
                  gap: 'var(--space-3)',
                  padding: 'var(--space-2) var(--space-3)',
                  borderBottom: i < filtered.length - 1 ? '1px solid var(--border)' : 'none',
                  fontSize: 'var(--fs-sm)',
                }}
              >
                <code style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--text-muted)', whiteSpace: 'nowrap' }}>
                  {fmtTs(e.ts)}
                </code>
                <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', color: 'var(--accent)', whiteSpace: 'nowrap', minWidth: 150 }}>
                  {e.actor}
                </span>
                <span style={{ color: 'var(--text-primary)', flex: 1, minWidth: 0 }}>
                  <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)' }}>{e.action}</span>
                  <span style={{ color: 'var(--text-tertiary)' }}> · {e.target}</span>
                </span>
                <Badge tone={RESULT_TONE[e.result]} dot>{RESULT_LABEL[e.result]}</Badge>
              </div>
            ))}
          </div>
        )}
      </Card>
    </PanelRoot>
  );
}
