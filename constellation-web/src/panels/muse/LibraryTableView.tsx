// MGUI-02 (S129): the library management table — guide screen 03. Rendered inside the Library
// panel behind the guide's Grid⇄Table toggle rather than as its own rail entry, because the guide
// presents it as the same screen in a different presentation.
//
// The guide's columns are Title · Year · Kind · Quality profile · On disk / cutoff · Size · Status,
// with "mono figures" and status badges. `GET /api/library/table` is PUBLIC and supplies all of it.
//
// The load-bearing correctness rule here is the UPGRADE SIGNAL. The guide shows
// "On disk / cutoff" as the upgrade cue, and `cutoff_met` is `null` on this deployment for every
// row (no quality profiles configured). A null is NOT "meets cutoff" and it is NOT "needs
// upgrade" — it is "unknown". Rendering it as either would invent a quality judgement about the
// operator's files, so unknown renders as an em-dash and no badge.
import type { MuseLibraryTableRow } from '../../hooks/useMuse';

/** Bytes → a compact mono figure. Returns an em-dash for null/0 rather than "0 B", which would
 *  claim a zero-byte file for a title whose size simply was not recorded. */
function formatSize(bytes: number | null): string {
  if (bytes === null || bytes === 0) return '—';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let v = bytes;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u += 1;
  }
  return `${v >= 10 || u === 0 ? Math.round(v) : v.toFixed(1)} ${units[u]}`;
}

/** The guide's status vocabulary, derived from what the row actually asserts. */
function statusOf(row: MuseLibraryTableRow): { text: string; tone: string } {
  if (!row.on_disk) return { text: 'Wanted', tone: 'var(--info, #60a5fa)' };
  // `cutoff_met === false` is a POSITIVE assertion that the file is below cutoff — the guide's
  // "Upgrade available". Only shown when the backend actually said so.
  if (row.cutoff_met === false) return { text: 'Upgrade available', tone: 'var(--warn, #fbbf24)' };
  return { text: 'On disk', tone: 'var(--ok, #4ade80)' };
}

const TH: React.CSSProperties = {
  textAlign: 'left',
  padding: '4px 8px',
  fontSize: 'var(--fs-2xs, 10px)',
  fontFamily: 'var(--font-mono)',
  textTransform: 'uppercase',
  letterSpacing: '0.04em',
  color: 'var(--text-300)',
  borderBottom: '1px solid var(--border)',
  position: 'sticky',
  top: 0,
  background: 'var(--space-800, var(--space-700, #0b0b10))',
  whiteSpace: 'nowrap',
};

const TD: React.CSSProperties = {
  padding: '4px 8px',
  fontSize: 'var(--fs-xs)',
  color: 'var(--text-100)',
  borderBottom: '1px solid var(--border-subtle, rgba(255,255,255,0.05))',
  whiteSpace: 'nowrap',
};

/** Mono + tabular figures, per the guide's "mono figures" pattern — so columns of numbers line up. */
const TD_NUM: React.CSSProperties = {
  ...TD,
  fontFamily: 'var(--font-mono)',
  fontVariantNumeric: 'tabular-nums',
  textAlign: 'right',
};

export function LibraryTableView({ rows }: { rows: MuseLibraryTableRow[] }) {
  return (
    // Horizontal overflow scrolls INSIDE the table container, and the region is focusable +
    // labelled so it is reachable by keyboard (a scrollable region that cannot be focused is
    // unreachable without a pointer).
    <div
      role="region"
      aria-label="Library management table"
      tabIndex={0}
      style={{ height: '100%', minHeight: 0, overflow: 'auto' }}
    >
      <table style={{ borderCollapse: 'collapse', width: '100%', minWidth: 720 }}>
        <thead>
          <tr>
            <th style={TH}>Title</th>
            <th style={TH}>Year</th>
            <th style={TH}>Kind</th>
            <th style={TH}>Quality profile</th>
            {/* The guide's upgrade-signal column, verbatim: "On disk / cutoff". codex correctly
                noted that rendering a Files count instead dropped a guide element that IS
                buildable — both `on_disk` and `cutoff_met` are in the projection. */}
            <th style={TH}>On disk / cutoff</th>
            <th style={{ ...TH, textAlign: 'right' }}>Files</th>
            <th style={{ ...TH, textAlign: 'right' }}>Size</th>
            <th style={TH}>Status</th>
          </tr>
        </thead>
        <tbody>
          {rows.map(r => {
            const status = statusOf(r);
            return (
              <tr key={r.media_item_id}>
                <td style={{ ...TD, maxWidth: 340, overflow: 'hidden', textOverflow: 'ellipsis' }} title={r.title}>
                  {r.title}
                </td>
                {/* An absent year is an em-dash, never the string "null". */}
                <td style={TD_NUM}>{r.year ?? '—'}</td>
                <td style={{ ...TD, color: 'var(--text-200)' }}>{r.kind}</td>
                {/* No profile configured → em-dash. Do NOT substitute a default profile name. */}
                <td style={{ ...TD, color: 'var(--text-200)' }}>{r.quality_profile_name ?? '—'}</td>
                {/* on_disk is a definite boolean; cutoff_met is TRISTATE and the null case is the
                    whole point — `null` means no quality profile is configured, i.e. UNKNOWN. It
                    renders as an em-dash with no badge: reading it as "meets cutoff" would invent a
                    quality judgement about the operator's file, and reading it as "needs upgrade"
                    would invent a defect. On this deployment every row is null. */}
                <td style={{ ...TD, fontFamily: 'var(--font-mono)', color: 'var(--text-200)' }}>
                  <span style={{ color: r.on_disk ? 'var(--ok, #4ade80)' : 'var(--text-300)' }}>
                    {r.on_disk ? '✓' : '✗'}
                  </span>
                  {' / '}
                  <span
                    title={
                      r.cutoff_met === null
                        ? 'No quality profile configured — cutoff unknown'
                        : r.cutoff_met
                          ? 'Meets cutoff'
                          : 'Below cutoff — upgrade available'
                    }
                    style={{
                      color:
                        r.cutoff_met === null
                          ? 'var(--text-300)'
                          : r.cutoff_met
                            ? 'var(--ok, #4ade80)'
                            : 'var(--warn, #fbbf24)',
                    }}
                  >
                    {r.cutoff_met === null ? '—' : r.cutoff_met ? '✓' : '↑'}
                  </span>
                </td>
                <td style={TD_NUM}>{r.file_count}</td>
                <td style={TD_NUM}>{formatSize(r.size_bytes)}</td>
                <td style={{ ...TD, color: status.tone, fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-2xs, 10px)' }}>
                  {status.text}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
