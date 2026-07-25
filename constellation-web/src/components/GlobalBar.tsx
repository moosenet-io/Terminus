// CONST-16 / S127 TGUI2: the two-tier shell's top bar (guide-spec §3.1). The persistent global
// frame: wordmark (violet node dot + "terminus") · the FIVE constellation CORE tabs (Lumina /
// Chord / Terminus / Harmony / Muse, active = violet-filled pill) · a search field
// ("search tools… ⌘K") · a Comfortable/Compact density toggle (segmented, active violet) · an
// account circle.
//
// S127 note — this replaces CGUI-12's fictional 3-"crate" strip (lumina-core/chord-proxy/
// terminus-rs) with the REAL constellation members (see lib/cores.ts). Each core's node dot is
// coloured by its kind (violet core / green endpoint). Selecting a core scopes the left rail +
// card canvas to that core's member modules; Terminus owns Models + MINT as sub-sections.
//
// CONST-25: the ⌘K search button just calls `onOpenPalette` — the palette's own open state,
// keyboard shortcut, and markup live in App.tsx's Shell + `CommandPalette.tsx`, so Ctrl/Cmd+K
// works everywhere the shell is mounted, not only while this bar has focus.
import type { FeedItem } from '../lib/activityFeed';
import type { CoreDescriptor, CoreId } from '../lib/cores';
import { coreKind } from '../lib/cores';
import { KIND_COLOR } from '../panels/overview/moduleMeta';
import { Wordmark } from './Wordmark';
import { NotificationBell } from './NotificationBell';

export type Density = 'comfortable' | 'compact';

interface GlobalBarProps {
  /** The five constellation cores rendered as tabs. */
  cores: readonly CoreDescriptor[];
  activeCoreId: CoreId;
  onSelectCore: (id: CoreId) => void;
  density: Density;
  onDensityChange: (d: Density) => void;
  username?: string | null;
  onLogout?: () => void;
  /** True when the last health poll failed outright (network/backend down); the bar shows a
   *  degraded indicator while continuing to render the last known state (edge case §10). */
  pollDegraded: boolean;
  /** Present only in the <760px "drawer" rail variant — renders a menu trigger before the
   *  wordmark that opens the module rail drawer. */
  onOpenMenu?: () => void;
  /** CONST-25: opens the full CommandPalette (owned by App.tsx's Shell). */
  onOpenPalette: () => void;
  /** CONST-26 (§3.3): the shell's merged activity feed — backs the bell menu here. Optional so
   *  every existing caller keeps compiling untouched; the bell simply doesn't render when omitted. */
  feedItems?: FeedItem[];
}

/** First (uppercase) glyph of the account label, for the account circle. */
function accountInitial(username?: string | null): string {
  const c = (username ?? '').trim()[0];
  return c ? c.toUpperCase() : '@';
}

export function GlobalBar({
  cores,
  activeCoreId,
  onSelectCore,
  density,
  onDensityChange,
  username,
  onLogout,
  pollDegraded,
  onOpenMenu,
  onOpenPalette,
  feedItems,
}: GlobalBarProps) {
  return (
    <div
      style={{
        position: 'relative',
        zIndex: 2,
        display: 'flex',
        alignItems: 'center',
        gap: 'var(--space-4)',
        padding: '0 var(--space-4)',
        height: 52,
        flexShrink: 0,
        borderBottom: '1px solid var(--border-subtle)',
        // Translucent so the fixed deep-space backdrop reads through the bar (guide §0 frame).
        background: 'linear-gradient(180deg, rgba(22,17,44,0.72), rgba(13,11,26,0.55))',
        backdropFilter: 'blur(8px)',
      }}
    >
      {onOpenMenu && (
        <button
          onClick={onOpenMenu}
          aria-label="Open module navigation"
          style={{
            background: 'none',
            border: '1px solid var(--border-default)',
            borderRadius: 'var(--radius-md)',
            color: 'var(--text-secondary)',
            width: 28,
            height: 28,
            cursor: 'pointer',
            flexShrink: 0,
          }}
        >
          ☰
        </button>
      )}

      {/* Wordmark → the active core's overview (App wires onSelectCore to also navigate). */}
      <button
        onClick={() => onSelectCore(activeCoreId)}
        style={{ background: 'none', border: 'none', cursor: 'pointer', padding: 0, flexShrink: 0 }}
        aria-label="Go to overview"
      >
        <Wordmark />
      </button>

      {/* Core tabs — active = violet-filled pill (§3.1). Exposed as an ARIA tablist so a
          screen reader announces the selected core (aria-selected) and the tab count. Each dot
          takes the core's kind colour (violet core / green endpoint) so the row reads
          semantically; the active tab additionally glows. */}
      <nav
        role="tablist"
        aria-label="Cores"
        style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', flex: 1, overflowX: 'auto' }}
      >
        {cores.map(c => {
          const active = c.id === activeCoreId;
          const dot = KIND_COLOR[coreKind(c.id)];
          return (
            <button
              key={c.id}
              role="tab"
              onClick={() => onSelectCore(c.id)}
              aria-selected={active}
              aria-current={active ? 'page' : undefined}
              style={{
                display: 'flex',
                alignItems: 'center',
                gap: 'var(--space-2)',
                background: active ? 'var(--accent-soft)' : 'transparent',
                border: active ? '1px solid var(--border-emphasis)' : '1px solid transparent',
                borderRadius: 'var(--radius-pill)',
                cursor: 'pointer',
                padding: 'var(--space-1) var(--space-3)',
                color: active ? 'var(--text-primary)' : 'var(--text-tertiary)',
                fontFamily: 'var(--font-mono)',
                fontSize: 'var(--fs-mono-sm)',
                letterSpacing: 'var(--ls-mono)',
                fontWeight: active ? 600 : 400,
                whiteSpace: 'nowrap',
              }}
            >
              <span
                aria-hidden
                style={{
                  width: 6,
                  height: 6,
                  borderRadius: '50%',
                  background: active ? dot : 'var(--text-500)',
                  boxShadow: active ? `0 0 7px ${dot}` : 'none',
                  flexShrink: 0,
                }}
              />
              {c.title}
            </button>
          );
        })}
      </nav>

      {/* Search field / ⌘K palette trigger. */}
      <button
        onClick={onOpenPalette}
        aria-label="Search tools"
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 'var(--space-2)',
          background: 'var(--bg-surface)',
          border: '1px solid var(--border-default)',
          color: 'var(--text-tertiary)',
          borderRadius: 'var(--radius-md)',
          padding: 'var(--space-1) var(--space-3)',
          fontSize: 'var(--text-sm)',
          cursor: 'pointer',
          flexShrink: 0,
        }}
      >
        search tools…{' '}
        <kbd style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)' }}>⌘K</kbd>
      </button>

      {/* Density toggle — segmented, active = violet (§3.1). */}
      <div
        role="group"
        aria-label="Density"
        style={{
          display: 'flex',
          border: '1px solid var(--border-default)',
          borderRadius: 'var(--radius-md)',
          overflow: 'hidden',
          flexShrink: 0,
        }}
      >
        {(['comfortable', 'compact'] as const).map(d => (
          <button
            key={d}
            onClick={() => onDensityChange(d)}
            aria-pressed={density === d}
            style={{
              padding: 'var(--space-1) var(--space-2)',
              fontSize: 'var(--text-xs)',
              border: 'none',
              cursor: 'pointer',
              textTransform: 'capitalize',
              background: density === d ? 'var(--accent-soft)' : 'transparent',
              color: density === d ? 'var(--accent-primary)' : 'var(--text-tertiary)',
            }}
          >
            {d}
          </button>
        ))}
      </div>

      <div style={{ display: 'flex', alignItems: 'center', gap: 'var(--space-2)', flexShrink: 0 }}>
        {feedItems && <NotificationBell items={feedItems} />}
        {pollDegraded && (
          <span
            title="Health poll degraded — showing last known status"
            aria-label="Health poll degraded"
            style={{ color: 'var(--status-warning)', fontSize: 'var(--text-sm)' }}
          >
            ⚠
          </span>
        )}
        {/* Account circle — blue avatar with the account initial; a plain button so Sign out is
            still reachable (title carries the full username). */}
        {onLogout ? (
          <button
            onClick={onLogout}
            title={username ? `${username} — sign out` : 'Sign out'}
            aria-label={username ? `${username}, sign out` : 'Sign out'}
            style={{
              width: 28,
              height: 28,
              borderRadius: '50%',
              flexShrink: 0,
              background: 'rgba(59,130,246,0.18)',
              border: '1px solid var(--node-source)',
              color: 'var(--flux-blue-soft)',
              fontFamily: 'var(--font-sans)',
              fontWeight: 600,
              fontSize: 'var(--fs-sm)',
              cursor: 'pointer',
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'center',
            }}
          >
            {accountInitial(username)}
          </button>
        ) : (
          <span
            aria-hidden
            style={{
              width: 28,
              height: 28,
              borderRadius: '50%',
              flexShrink: 0,
              background: 'rgba(59,130,246,0.18)',
              border: '1px solid var(--node-source)',
              color: 'var(--flux-blue-soft)',
              fontFamily: 'var(--font-sans)',
              fontWeight: 600,
              fontSize: 'var(--fs-sm)',
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'center',
            }}
          >
            {accountInitial(username)}
          </span>
        )}
      </div>
    </div>
  );
}
