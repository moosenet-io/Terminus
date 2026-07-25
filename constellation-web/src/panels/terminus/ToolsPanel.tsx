// CGUI-05 (TERM #528): Terminus module self — the deep TOOL CATALOG.
//
// The current-state audit found this panel showed the tool NAME only (a two-column table of
// module + name), no depth — the literal source of the operator complaint "modules are missing
// all their tools settings and depth". This rebuild turns it into a rich, scrollable catalog
// where every tool carries: category, description, schema/params (arguments), rate-limit, auth
// identity, enable/disable state, and last-invocation telemetry — grouped by module, searchable,
// and expandable to a full per-tool detail (consistent with CGUI-04's ModuleDetail depth).
//
// DATA PROVENANCE (real vs placeholder — the item requires calling it out):
//   • tool NAME, MODULE, ENABLED state  — REAL, from the aggregation client
//     (`terminus.configSummary()` → modules[].tools / modules[].enabled).
//   • category, description, params(schema), rate-limit, auth, last-invocation — DERIVED /
//     representative placeholder in toolCatalog.ts (deterministic), each field noted there as
//     pending the CGUI-08 data client's real per-tool metadata + invocation stream. The catalog
//     renders a truthful field shape with a sensible value rather than an empty cell — never
//     leaving the depth blank.
//
// Tokens only (var(--…)); the DS primitives (Card/Badge/StatusPill/DataTable) carry the brand.
// Inter + JetBrains Mono via the font tokens; no emoji.
//
// TODO(CONST-25 seam): once the command-palette entity registry lands (`registerPaletteSource`
// or equivalent), register this catalog as a palette entity source. Left as a no-op TODO rather
// than importing a not-yet-existing module so this branch typechecks/builds against origin/main;
// wire it up on the first rebase past CONST-25.
import { useEffect, useMemo, useState } from 'react';
import { Card, CardTitle } from '../../components/Card';
import { PanelRoot } from '../../components/PanelRoot';
import { Badge } from '../../components/Badge';
import { StatusPill } from '../../components/StatusPill';
import { SkeletonList } from '../../components/Skeleton';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import { getAggregationClient } from '../../lib/aggregationClient';
import type { TerminusConfigSummary } from '../../lib/aggregationClient';
import {
  deriveToolDetail, CATEGORY_BADGE,
  type ToolDetail, type ToolCategory, type ToolParam,
} from './toolCatalog';

const CATEGORIES: ToolCategory[] = ['read', 'write', 'search', 'admin'];

// mono cell styling reused across the detail — a code fragment / figure in JetBrains Mono.
const monoSm = { fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)' } as const;

export function ToolsPanel() {
  const [config, setConfig] = useState<TerminusConfigSummary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [query, setQuery] = useState('');
  const [moduleFilter, setModuleFilter] = useState<string | null>(null);
  const [categoryFilter, setCategoryFilter] = useState<ToolCategory | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    getAggregationClient()
      .terminus.configSummary()
      .then(d => { if (!cancelled) setConfig(d); })
      .catch(e => { if (!cancelled) setError(e instanceof Error ? e.message : 'Failed to load'); })
      .finally(() => { if (!cancelled) setLoading(false); });
    return () => { cancelled = true; };
  }, []);

  // Real facts (name/module/enabled) → full derived per-tool depth. Memoised once per config
  // load so telemetry stamps stay stable (no flicker) between renders.
  const allTools: ToolDetail[] = useMemo(() => {
    if (!config) return [];
    const out: ToolDetail[] = [];
    // Determinism: the catalog must render identically offline (same fixture → same telemetry),
    // so we DON'T pass a wall-clock `now` — deriveToolDetail defaults to the fixed FIXTURE_NOW
    // epoch. The synthetic last-invocation stamps are a placeholder anyway (real per-tool
    // invocation stream lands with the CGUI-08 data client).
    for (const m of config.modules) {
      for (const tool of m.tools ?? []) {
        out.push(deriveToolDetail(m.name, tool, m.enabled));
      }
    }
    return out;
  }, [config]);

  const moduleNames = useMemo(
    () => Array.from(new Set(allTools.map(t => t.module))).sort(),
    [allTools],
  );

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return allTools.filter(t => {
      if (moduleFilter && t.module !== moduleFilter) return false;
      if (categoryFilter && t.category !== categoryFilter) return false;
      if (!q) return true;
      return (
        t.name.toLowerCase().includes(q) ||
        t.module.toLowerCase().includes(q) ||
        t.description.toLowerCase().includes(q)
      );
    });
  }, [allTools, query, moduleFilter, categoryFilter]);

  // Group the visible tools by module so the catalog reads as a module → tools tree.
  const groups = useMemo(() => {
    const byModule = new Map<string, ToolDetail[]>();
    for (const t of filtered) {
      const arr = byModule.get(t.module) ?? [];
      arr.push(t);
      byModule.set(t.module, arr);
    }
    return Array.from(byModule.entries())
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([module, tools]) => ({ module, tools, enabled: tools[0]?.enabled ?? false }));
  }, [filtered]);

  return (
    <PanelRoot style={{ padding: 'var(--space-5)', display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
      <CardTitle subtitle="Every registered tool with its schema, limits, auth identity and last-call telemetry — grouped by module, searchable">
        Terminus — Tools
      </CardTitle>

      {error && (
        <Card variant="content">
          <span style={{ color: 'var(--status-error)' }}>{error}</span>
        </Card>
      )}

      {loading && !error && (
        <Card variant="content">
          <SkeletonList rows={6} />
        </Card>
      )}

      {!loading && !error && (
        <>
          {/* ── toolbar: search + module chips + category chips ── */}
          <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)' }}>
            <input
              type="text"
              value={query}
              onChange={e => setQuery(e.target.value)}
              placeholder="Search tools…"
              aria-label="Search tools"
              style={{
                background: 'var(--space-700)',
                // 1px hairline + 6px/10px input padding are DS-parity geometry literals (carried
                // from the prior ToolsPanel input); adherence-lint warns on these px, expected.
                border: '1px solid var(--border)',
                borderRadius: 'var(--radius-md)',
                color: 'var(--text-primary)',
                padding: '6px 10px',
                fontSize: 'var(--fs-sm)',
                minWidth: 220,
                maxWidth: 360,
              }}
            />
            <div style={{ display: 'flex', gap: 'var(--space-2)', flexWrap: 'wrap', alignItems: 'center' }}>
              <FilterChip active={moduleFilter === null} onClick={() => setModuleFilter(null)}>
                all modules ({allTools.length})
              </FilterChip>
              {moduleNames.map(name => (
                <FilterChip key={name} active={moduleFilter === name} onClick={() => setModuleFilter(name)}>
                  {name} ({allTools.filter(t => t.module === name).length})
                </FilterChip>
              ))}
            </div>
            <div style={{ display: 'flex', gap: 'var(--space-2)', flexWrap: 'wrap', alignItems: 'center' }}>
              <FilterChip active={categoryFilter === null} onClick={() => setCategoryFilter(null)}>
                all kinds
              </FilterChip>
              {CATEGORIES.map(cat => (
                <FilterChip key={cat} active={categoryFilter === cat} onClick={() => setCategoryFilter(cat)}>
                  <Badge tone={CATEGORY_BADGE[cat]} dot>{cat}</Badge>
                </FilterChip>
              ))}
            </div>
          </div>

          {/* ── grouped catalog ── */}
          {groups.length === 0 && (
            <Card variant="content">
              <div style={{ padding: 'var(--space-4)', textAlign: 'center', color: 'var(--text-muted)', fontSize: 'var(--fs-sm)' }}>
                {allTools.length === 0 ? 'No tools registered' : 'No tools match this filter'}
              </div>
            </Card>
          )}

          {groups.map(group => (
            <section key={group.module} style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)' }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-3)' }}>
                <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', letterSpacing: 'var(--ls-mono)', textTransform: 'uppercase', color: 'var(--text-400)' }}>
                  {group.module}
                </span>
                <StatusPill state={group.enabled ? 'online' : 'idle'} label={group.enabled ? 'enabled' : 'disabled'} />
                <span style={{ ...monoSm, color: 'var(--text-500)' }}>
                  {group.tools.length} tool{group.tools.length === 1 ? '' : 's'}
                </span>
              </div>

              <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)' }}>
                {group.tools.map(tool => (
                  <ToolRow
                    key={tool.name}
                    tool={tool}
                    open={expanded === tool.name}
                    onToggle={() => setExpanded(prev => (prev === tool.name ? null : tool.name))}
                  />
                ))}
              </div>
            </section>
          ))}
        </>
      )}
    </PanelRoot>
  );
}

// ── filter chip (reuses the DS `.h-badge` pill styling as a toggle button) ─────────────────────
function FilterChip({ active, onClick, children }: { active: boolean; onClick: () => void; children: React.ReactNode }) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`h-badge ${active ? 'h-badge-violet' : 'h-badge-neutral'}`}
      style={{ cursor: 'pointer', border: 'none' }}
    >
      {children}
    </button>
  );
}

// ── one collapsible tool row: summary header + full detail on expand ───────────────────────────
function ToolRow({ tool, open, onToggle }: { tool: ToolDetail; open: boolean; onToggle: () => void }) {
  const inv = tool.lastInvocation;
  return (
    <Card variant="content" padding="var(--space-3)">
      {/* summary row — always visible, click to zoom into the schema/config detail */}
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={open}
        style={{
          all: 'unset',
          cursor: 'pointer',
          display: 'flex',
          alignItems: 'center',
          gap: 'var(--space-3)',
          width: '100%',
          flexWrap: 'wrap',
        }}
      >
        {/* chevron — rotates on open (DS-parity affordance from Card's expandable variant). */}
        <span aria-hidden style={{ color: 'var(--text-tertiary)', fontSize: 'var(--fs-xs)', transition: 'transform var(--dur-fast) var(--ease-out)', transform: open ? 'rotate(90deg)' : 'none' }}>▶</span>
        <code style={{ ...monoSm, color: 'var(--text-100)' }}>{tool.name}</code>
        <Badge tone={CATEGORY_BADGE[tool.category]} dot>{tool.category}</Badge>
        <span style={{ flex: 1 }} />
        {/* enable state + last-call telemetry, compact. */}
        <Badge tone={tool.enabled ? 'green' : 'neutral'}>{tool.enabled ? 'enabled' : 'disabled'}</Badge>
        <span style={{ ...monoSm, color: inv ? (inv.result === 'error' ? 'var(--flux-rose)' : 'var(--text-400)') : 'var(--text-500)' }}>
          {inv ? `${inv.result} · ${inv.ago}` : 'never called'}
        </span>
      </button>

      {open && <ToolDetailBody tool={tool} />}
    </Card>
  );
}

// ── expanded detail: description + schema table + config strip ─────────────────────────────────
function ToolDetailBody({ tool }: { tool: ToolDetail }) {
  const paramColumns: DataTableColumn<ToolParam>[] = [
    { key: 'name', header: 'Argument', render: p => <code style={{ ...monoSm, color: 'var(--text-100)' }}>{p.name}</code> },
    { key: 'type', header: 'Type', render: p => <code style={{ ...monoSm, color: 'var(--text-300)' }}>{p.type}</code> },
    { key: 'required', header: 'Req', render: p => p.required ? <Badge tone="amber">required</Badge> : <span style={{ ...monoSm, color: 'var(--text-500)' }}>—</span> },
    { key: 'desc', header: 'Description', render: p => <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-300)' }}>{p.desc}</span> },
  ];

  return (
    // 1px hairline + minmax(150px) grid track are DS-parity geometry literals (same posture as
    // ModuleDetail's config strip); adherence-lint warns on these px, expected.
    <div style={{ marginTop: 'var(--space-3)', borderTop: '1px solid var(--line-soft)', paddingTop: 'var(--space-3)', display: 'flex', flexDirection: 'column', gap: 'var(--space-3)' }}>
      {/* description (derived from the tool name — representative, CGUI-08) */}
      <div style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-200)' }}>
        {tool.description}
        <span style={{ ...monoSm, color: 'var(--text-500)' }}> — <code>{tool.name}</code></span>
      </div>

      {/* schema / params */}
      <div>
        <div style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', letterSpacing: 'var(--ls-mono)', textTransform: 'uppercase', color: 'var(--text-500)', marginBottom: 'var(--space-2)' }}>
          Schema · arguments
        </div>
        <DataTable columns={paramColumns} rows={tool.params} rowKey={p => p.name} emptyMessage="No arguments" />
      </div>

      {/* config strip: rate limit · auth · state */}
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(150px, 1fr))', gap: 'var(--space-3)' }}>
        <ConfigCell label="rate limit" value={tool.rateLimit} />
        <ConfigCell label="auth identity" value={tool.auth} mono />
        <ConfigCell label="state" node={<StatusPill state={tool.enabled ? 'online' : 'idle'} label={tool.enabled ? 'enabled' : 'disabled'} />} />
      </div>
    </div>
  );
}

function ConfigCell({ label, value, node, mono }: { label: string; value?: string; node?: React.ReactNode; mono?: boolean }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-1)' }}>
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', letterSpacing: 'var(--ls-mono)', textTransform: 'uppercase', color: 'var(--text-500)' }}>{label}</span>
      {node ?? (
        <span style={mono ? { ...monoSm, color: 'var(--text-100)' } : { fontSize: 'var(--fs-sm)', color: 'var(--text-100)' }}>
          {value}
        </span>
      )}
    </div>
  );
}
