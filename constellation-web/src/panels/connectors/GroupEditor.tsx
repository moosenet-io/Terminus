// RMCP-13 (TERM-624): the tool-group editor with a LIVE match preview.
//
// A group is a list of patterns, and a pattern is only meaningful in terms of what it currently
// selects. So every edit re-asks the server ("given these patterns, what matches?") and shows
// the answer as the operator types — debounced, because it is a real call, not a local match.
//
// The preview comes from `rmcp_group_preview`, i.e. the SAME matcher that resolves the group at
// list/call time (RMCP-06). This file contains no pattern matching. `patternHint` next to the
// input is a typing aid with no authority: the server rejects invalid patterns at write time
// and its rejections are shown verbatim, including the ones the hint did not anticipate (a bare
// `*`, for example, is valid for an operator-owned group and refused for a delegated one — a
// question about ownership that only the server can answer).
import { useEffect, useMemo, useRef, useState } from 'react';
import { Badge } from '../../components/Badge';
import { Button } from '../../components/Button';
import { Card } from '../../components/Card';
import { DataTable } from '../../components/DataTable';
import type { DataTableColumn } from '../../components/DataTable';
import { EmptyState } from '../../components/EmptyState';
import { RoleGate } from '../../components/RoleGate';
import { createGroup, describeRmcpError, previewGroup, updateGroup } from '../../lib/rmcpClient';
import type { RmcpGroupPreview, RmcpResolvedTool, RmcpToolGroup } from '../../types/rmcp';
import { parseLines, patternHint } from './connectorForm';

/** Debounce for the live preview — long enough that a burst of keystrokes is one call, short
 *  enough that the answer feels attached to the edit. */
const PREVIEW_DEBOUNCE_MS = 350;
/** Rows rendered per page of the preview; the request itself is separately bounded. */
const PREVIEW_PAGE = 20;

const mono = { fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)' } as const;

const labelStyle = {
  fontFamily: 'var(--font-mono)',
  fontSize: 'var(--fs-mono-sm)',
  letterSpacing: 'var(--ls-mono)',
  textTransform: 'uppercase',
  color: 'var(--text-500)',
} as const;

const inputStyle = {
  width: '100%',
  padding: 'var(--space-2) var(--space-3)',
  borderRadius: 'var(--radius-sm)',
  border: 'var(--border-width) solid var(--border)',
  background: 'var(--bg-elevated)',
  color: 'var(--text-100)',
  fontFamily: 'var(--font-mono)',
  fontSize: 'var(--fs-mono)',
} as const;

export interface GroupEditorProps {
  /** The group being edited, or null when composing a new one. */
  group: RmcpToolGroup | null;
  onSaved: (group: RmcpToolGroup) => void;
  onCancel: () => void;
  /** Offered when the server reports a concurrent edit. */
  onReload: () => void;
}

export function GroupEditor({ group, onSaved, onCancel, onReload }: GroupEditorProps) {
  const [name, setName] = useState(group?.name ?? '');
  const [description, setDescription] = useState(group?.description ?? '');
  const [patternText, setPatternText] = useState((group?.patterns ?? []).join('\n'));
  const [preview, setPreview] = useState<RmcpGroupPreview | null>(null);
  const [previewFailure, setPreviewFailure] = useState<string | null>(null);
  const [previewing, setPreviewing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [failure, setFailure] = useState<{ message: string; conflict: boolean } | null>(null);
  const [page, setPage] = useState(0);
  /** Monotonic request id — a slow earlier preview must never overwrite a newer answer. */
  const previewSeq = useRef(0);

  useEffect(() => {
    setName(group?.name ?? '');
    setDescription(group?.description ?? '');
    setPatternText((group?.patterns ?? []).join('\n'));
    setFailure(null);
    setPage(0);
  }, [group]);

  const patterns = useMemo(() => parseLines(patternText), [patternText]);
  const hints = useMemo(
    () =>
      patterns
        .map(p => ({ pattern: p, hint: patternHint(p) }))
        .filter((x): x is { pattern: string; hint: string } => x.hint !== null),
    [patterns],
  );

  // Live preview: ask the server what these patterns select, on a debounce.
  useEffect(() => {
    if (patterns.length === 0) {
      setPreview(null);
      setPreviewFailure(null);
      return;
    }
    const seq = ++previewSeq.current;
    setPreviewing(true);
    const timer = setTimeout(() => {
      previewGroup(patterns)
        .then(p => {
          if (seq === previewSeq.current) {
            setPreview(p);
            setPreviewFailure(null);
            setPage(0);
          }
        })
        .catch(e => {
          if (seq === previewSeq.current) {
            setPreview(null);
            setPreviewFailure(describeRmcpError(e).message);
          }
        })
        .finally(() => {
          if (seq === previewSeq.current) setPreviewing(false);
        });
    }, PREVIEW_DEBOUNCE_MS);
    return () => clearTimeout(timer);
    // `patterns` is rebuilt per keystroke; joining gives a stable dependency for the same list.
  }, [patterns.join('\n')]); // eslint-disable-line react-hooks/exhaustive-deps

  const save = () => {
    setSaving(true);
    setFailure(null);
    const done = (g: RmcpToolGroup) => onSaved(g);
    const fail = (e: unknown) => {
      const d = describeRmcpError(e);
      setFailure({ message: d.message, conflict: d.kind === 'conflict' });
    };
    const p = group
      ? updateGroup({ id: group.id, version: group.version, name: name.trim(), description, patterns })
      : createGroup({ name: name.trim(), description, patterns });
    p.then(done).catch(fail).finally(() => setSaving(false));
  };

  const columns: DataTableColumn<RmcpResolvedTool>[] = [
    { key: 'name', header: 'Tool', render: t => <code style={{ ...mono, color: 'var(--text-100)' }}>{t.name}</code> },
    { key: 'ns', header: 'Namespace', render: t => <span style={{ ...mono, color: 'var(--text-400)' }}>{t.namespace}</span> },
    { key: 'pattern', header: 'Matched by', render: t => <code style={{ ...mono, color: 'var(--text-300)' }}>{t.matchedPattern}</code> },
    {
      key: 'state',
      header: 'State',
      align: 'right',
      render: t => (t.available ? <Badge tone="green" dot>reachable</Badge> : <Badge tone="amber" dot>unavailable</Badge>),
    },
  ];

  const rows = preview?.tools ?? [];
  const pageRows = rows.slice(page * PREVIEW_PAGE, page * PREVIEW_PAGE + PREVIEW_PAGE);
  const readOnly = group !== null && !group.editable;

  return (
    <Card variant="content">
      <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-4)' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-3)' }}>
          <span style={{ fontSize: 'var(--fs-h4)', color: 'var(--text-100)' }}>
            {group ? `Edit group — ${group.name}` : 'New tool group'}
          </span>
          {readOnly && <Badge tone="amber" dot>read-only — owned by another account</Badge>}
        </div>

        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(16rem, 1fr))', gap: 'var(--space-4)' }}>
          <label style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-1)' }}>
            <span style={labelStyle}>name</span>
            <input value={name} readOnly={readOnly} onChange={e => setName(e.target.value)} style={inputStyle} />
          </label>
          <label style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-1)' }}>
            <span style={labelStyle}>description</span>
            <input value={description} readOnly={readOnly} onChange={e => setDescription(e.target.value)} style={inputStyle} />
          </label>
        </div>

        <label style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-1)' }}>
          <span style={labelStyle}>patterns (one per line)</span>
          <textarea
            value={patternText}
            readOnly={readOnly}
            onChange={e => setPatternText(e.target.value)}
            rows={5}
            spellCheck={false}
            aria-label="Tool group patterns"
            style={{ ...inputStyle, resize: 'vertical' }}
          />
          <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-400)' }}>
            An exact tool name, a trailing <code style={mono}>*</code> prefix, or{' '}
            <code style={mono}>namespace::*</code>. No regex and no negation — denials live in the deny layer.
            An empty group matches nothing.
          </span>
        </label>

        {hints.length > 0 && (
          <ul style={{ margin: 0, paddingLeft: 'var(--space-4)', color: 'var(--status-warning)', fontSize: 'var(--fs-sm)' }}>
            {hints.map(h => (
              <li key={h.pattern}>
                <code style={mono}>{h.pattern}</code> — {h.hint}
              </li>
            ))}
          </ul>
        )}

        {failure && (
          <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-3)', flexWrap: 'wrap' }}>
            <span style={{ color: failure.conflict ? 'var(--status-warning)' : 'var(--status-error)', fontSize: 'var(--fs-sm)' }}>
              {failure.message}
            </span>
            {failure.conflict && <Button size="sm" variant="secondary" onClick={onReload}>Reload groups</Button>}
          </div>
        )}

        {/* ── live match preview ─────────────────────────────────────────── */}
        <div style={{ display: 'flex', flexDirection: 'column', gap: 'var(--space-2)' }}>
          <div style={{ display: 'flex', alignItems: 'baseline', gap: 'var(--space-3)', flexWrap: 'wrap' }}>
            <span style={{ fontSize: 'var(--fs-body)', color: 'var(--text-100)' }}>Matches right now</span>
            <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-400)' }}>
              Resolved by the server against the live catalog{previewing ? ' — updating…' : ''}
            </span>
            {preview && <Badge tone="neutral" mono>{rows.length}{preview.truncated ? '+' : ''} tools</Badge>}
          </div>

          {preview?.invalidPatterns.length ? (
            <ul style={{ margin: 0, paddingLeft: 'var(--space-4)', color: 'var(--status-error)', fontSize: 'var(--fs-sm)' }}>
              {preview.invalidPatterns.map(p => (
                <li key={p.pattern}>
                  <code style={mono}>{p.pattern}</code> — rejected by the server: {p.reason}
                </li>
              ))}
            </ul>
          ) : null}

          {previewFailure && <EmptyState title="Preview unavailable" message={previewFailure} compact />}

          {!previewFailure && patterns.length === 0 && (
            <EmptyState title="No patterns yet" message="Add a pattern to see exactly which tools it selects." compact />
          )}

          {!previewFailure && patterns.length > 0 && preview && (
            <>
              <DataTable
                columns={columns}
                rows={pageRows}
                rowKey={t => t.name}
                emptyMessage="These patterns match nothing in the current catalog."
              />
              {rows.length > PREVIEW_PAGE && (
                <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-3)' }}>
                  <Button size="sm" variant="ghost" disabled={page === 0} onClick={() => setPage(page - 1)}>Previous</Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    disabled={(page + 1) * PREVIEW_PAGE >= rows.length}
                    onClick={() => setPage(page + 1)}
                  >
                    Next
                  </Button>
                  <span style={{ ...mono, color: 'var(--text-500)' }}>
                    {page * PREVIEW_PAGE + 1}–{Math.min((page + 1) * PREVIEW_PAGE, rows.length)} of {rows.length}
                    {preview.truncated ? '+' : ''}
                  </span>
                </div>
              )}
              {preview.truncated && (
                <span style={{ fontSize: 'var(--fs-sm)', color: 'var(--text-400)' }}>
                  The server capped this preview — the real group matches at least this many.
                </span>
              )}
            </>
          )}
        </div>

        <div style={{ display: 'flex', gap: 'var(--space-3)', justifyContent: 'flex-end' }}>
          <Button variant="ghost" onClick={onCancel} disabled={saving}>Cancel</Button>
          {!readOnly && (
            <RoleGate>
              <Button variant="primary" onClick={save} disabled={saving || name.trim().length === 0}>
                {saving ? 'Saving…' : group ? 'Save group' : 'Create group'}
              </Button>
            </RoleGate>
          )}
        </div>
      </div>
    </Card>
  );
}
