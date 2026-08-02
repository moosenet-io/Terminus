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

describe('StatTile — three visually distinct states', () => {
  it('loading renders a placeholder glyph, never a value or a dash', () => {
    const html = renderToStaticMarkup(<StatTile label="Library size" state={{ kind: 'loading' }} />);
    expect(html).toContain('…');
    expect(html).not.toContain('>0<');
  });

  it('degraded renders "—" with the cause in a title attribute (never a fabricated 0)', () => {
    const html = renderToStaticMarkup(
      <StatTile label="Library size" state={{ kind: 'degraded', detail: 'HTTP 401 for /stats' }} />,
    );
    expect(html).toContain('—');
    expect(html).toContain('HTTP 401 for /stats');
    expect(html).not.toContain('>0<');
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
    expect(html).toContain('HTTP 401 for /stats');
    // The still-ready gaps-backlog tile ("17") must still be present.
    expect(html).toContain('17');
  });
});
