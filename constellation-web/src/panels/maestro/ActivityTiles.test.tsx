// MACT-06 (MUSE-126): component-level coverage for the Maestro Activity stat-tile row.
// Same convention as ActivityPanel.test.tsx (no jsdom/testing-library in this project) --
// `react-dom/server`'s `renderToStaticMarkup` on the PURE, hooks-free components (`StatTile`,
// `SeamTile`, `TileRow`), asserting on the resulting markup.
//
// THE REQUIRED TEST (this item's own TEST PLAN): an H2 placeholder tile renders the seam text
// and NEVER a "0"; a genuine 0 from a real endpoint renders as "0", not as "not reported".
// Those two must be DISTINGUISHABLE in the actual rendered output, not just individually true
// of two different pure functions -- so both are rendered side-by-side in one assertion below.
import { describe, it, expect } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import { SeamTile, StatTile, TileRow } from './ActivityTiles';
import type { TileRowStates } from './ActivityTiles';
import { MAESTRO_SEAM_LABEL } from './tileFormat';

/** Strips every HTML tag (and therefore every ATTRIBUTE — `title="..."` included) from a
 *  `renderToStaticMarkup` string, leaving only the text a person actually sees rendered.
 *
 *  Review finding (round 2, codex): the first cut of the degraded-tile test asserted
 *  `html.toContain(detail)`, which passed because the detail sat in an HTML `title` attribute
 *  — invisible without a hover, unavailable on touch, not reliably surfaced by assistive tech.
 *  That test would have stayed green forever with the bug it was meant to catch, because
 *  `title="HTTP 401 for /stats"` and a genuinely rendered `<div>HTTP 401 for /stats</div>` both
 *  satisfy a plain substring check on the raw markup. Asserting against `visibleText()` instead
 *  of the raw HTML is what makes a test here actually prove "a person can see this" rather than
 *  "this string exists somewhere in the DOM tree" — the exact distinction the review called out
 *  ("being present in the markup is not the same as being communicated"). */
function visibleText(html: string): string {
  return html.replace(/<[^>]+>/g, ' ');
}

describe('StatTile — three visually distinct states', () => {
  it('loading renders a placeholder glyph, never a value or a dash', () => {
    const html = renderToStaticMarkup(<StatTile label="Library size" state={{ kind: 'loading' }} />);
    expect(html).toContain('…');
    expect(html).not.toContain('>0<');
  });

  it('degraded renders "—" AND the cause as VISIBLE text — not only in a hover-only title attribute', () => {
    const html = renderToStaticMarkup(
      <StatTile label="Library size" state={{ kind: 'degraded', detail: 'HTTP 401 for /stats' }} />,
    );
    const visible = visibleText(html);
    // The dash is visible content.
    expect(visible).toContain('—');
    // THE finding this test exists to pin: the cause must be visible TEXT, not merely present
    // somewhere in the raw markup (a `title="..."` attribute would satisfy a raw `html.toContain`
    // check but disappear entirely once tags/attributes are stripped, which is what
    // `visibleText()` does). This must survive stripping.
    expect(visible).toContain('HTTP 401 for /stats');
    expect(html).not.toContain('>0<');
  });

  it('a long degraded detail is truncated in the VISIBLE line but kept in full in the title attribute', () => {
    const longDetail = 'HTTP 401 for /api/requests/queue (unauthenticated, CONSTELLATION_MUSE_TOKEN unset)';
    const html = renderToStaticMarkup(
      <StatTile label="Library size" state={{ kind: 'degraded', detail: longDetail }} />,
    );
    const visible = visibleText(html);
    // The full detail is still reachable on hover (a nice-to-have, never the sole carrier).
    expect(html).toContain(`title="${longDetail}"`);
    // But the VISIBLE line is shortened, not the raw 85-character string verbatim.
    expect(visible).not.toContain(longDetail);
    expect(visible).toContain('HTTP 401 for /api');
  });

  it('degraded and not-reported are distinguishable WITHOUT relying on colour alone', () => {
    // "Not reported" in this codebase is a `value` state whose formatted text is itself "—"
    // (e.g. formatRelativeTimestamp(null) -- see tileFormat.test.ts). Compare that render
    // against a `degraded` render of the SAME tile: colour (valueColor) differs, but the
    // review's point is that colour must not be the ONLY distinguishing signal. Proven here by
    // showing the VISIBLE TEXT differs too, independent of any colour/style inspection.
    const notReported = visibleText(
      renderToStaticMarkup(<StatTile label="Last ingest" state={{ kind: 'value', text: '—' }} />),
    );
    const degraded = visibleText(
      renderToStaticMarkup(<StatTile label="Last ingest" state={{ kind: 'degraded', detail: 'HTTP 401 for /stats' }} />),
    );
    expect(notReported).not.toEqual(degraded);
    expect(degraded).toContain('HTTP 401 for /stats');
    expect(notReported).not.toContain('HTTP 401 for /stats');
  });

  it('a genuine 0 value renders literally as "0" -- never coerced to "—"', () => {
    const html = renderToStaticMarkup(<StatTile label="Live streams" state={{ kind: 'value', text: '0' }} />);
    expect(html).toContain('0');
    // The dash glyph must not appear anywhere in a genuine-zero render.
    expect(html).not.toContain('—');
  });

  it('a nonzero value renders verbatim', () => {
    const html = renderToStaticMarkup(<StatTile label="Gaps backlog" state={{ kind: 'value', text: '17' }} />);
    expect(html).toContain('17');
  });
});

describe('SeamTile — the H2 (MACT-11) inert placeholder', () => {
  it('always renders the literal seam label, never a 0 and never a spinner glyph', () => {
    const html = renderToStaticMarkup(<SeamTile label="Host CPU / RAM" />);
    expect(html).toContain(MAESTRO_SEAM_LABEL);
    expect(html).not.toContain('>0<');
    expect(html).not.toContain('…');
  });
});

function readyStates(): TileRowStates {
  return {
    librarySize: { kind: 'value', text: '1842' },
    pendingItems: { kind: 'value', text: '2' },
    lastIngest: { kind: 'value', text: '45m ago' },
    gapsBacklog: { kind: 'value', text: '17' },
    subsystemWiring: { kind: 'value', text: '6 live of 9' },
    moduleHealth: { kind: 'value', text: '5 up of 5' },
    museHealth: { kind: 'value', text: 'ok · db up', tone: 'success' },
    // The load-bearing case: a REAL zero from a successful fetch.
    liveStreams: { kind: 'value', text: '0' },
  };
}

describe('TileRow — the seam vs. a genuine 0 are distinguishable in the same render', () => {
  it('renders a genuine live-stream 0 as "0" AND the H2 seam tiles as the seam label, never a 0 for either', () => {
    const html = renderToStaticMarkup(<TileRow states={readyStates()} />);

    // The genuine 0 (live streams) is present as a real "0" value.
    expect(html).toMatch(/0<\/div>/);

    // Every H2 tile renders the fixed seam label -- three times (Host CPU/RAM, Transcodes vs
    // cap, Scratch headroom) -- and NEVER substitutes a bare "0" for any of them.
    const seamOccurrences = html.split(MAESTRO_SEAM_LABEL).length - 1;
    expect(seamOccurrences).toBe(3);

    // Sanity: the seam tiles' labels are present too, so this is provably the H2 row and not a
    // coincidental string match.
    expect(html).toContain('Host CPU / RAM');
    expect(html).toContain('Transcodes vs cap');
    expect(html).toContain('Scratch headroom');
  });

  it('a degraded source renders its own tile as "—" without blanking a sibling ready tile', () => {
    const states = readyStates();
    states.librarySize = { kind: 'degraded', detail: 'HTTP 401 for /stats' };
    // pendingItems/lastIngest share the SAME /stats section in the real component, but this
    // pure render proves the row itself never couples one tile's failure to another's --
    // per-tile degradation is enforced by ActivityTiles' independent tileStateFromSection
    // calls, and this test pins that TileRow renders whatever state each tile is handed,
    // independently.
    const html = renderToStaticMarkup(<TileRow states={states} />);
    const visible = visibleText(html);
    // Visible, not just present in a title attribute -- see the StatTile tests above.
    expect(visible).toContain('HTTP 401 for /stats');
    // The still-ready gaps-backlog tile ("17") must still be present.
    expect(html).toContain('17');
  });
});
