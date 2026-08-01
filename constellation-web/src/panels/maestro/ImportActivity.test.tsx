// MACT-05 (MUSE-125): component-level regression coverage for the Import/acquisition section,
// same approach as ActivityPanel.test.tsx -- `react-dom/server`'s `renderToStaticMarkup`
// against the plain-props `ImportActivitySection`, no hooks/fetch mocking involved. Wrapped in
// a `MemoryRouter` because the wanted-count link renders a real `react-router-dom` `<Link>`.
import { describe, it, expect } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import { MemoryRouter } from 'react-router-dom';
import { ImportActivitySection } from './ImportActivity';
import type { MuseDownloadQueueRow, MuseWantedTitleRow } from '../../hooks/useMuse';

function queueRow(overrides: Partial<MuseDownloadQueueRow> = {}): MuseDownloadQueueRow {
  return {
    id: 1,
    request_id: null,
    monitored_item_id: 5,
    release_title: 'Some.Movie.2024.1080p.BluRay',
    indexer: 'Indexer1',
    protocol: 'torrent',
    status: 'downloading',
    size_bytes: 4_500_000_000,
    added_at: new Date().toISOString(),
    progress: null,
    ...overrides,
  };
}

function wantedRow(overrides: Partial<MuseWantedTitleRow> = {}): MuseWantedTitleRow {
  return {
    monitored_item_id: 1,
    media_metadata_id: 1,
    library_id: 1,
    kind: 'movie',
    title: 'Some Wanted Movie',
    year: 2024,
    poster_url: '/art/media_item/1',
    ...overrides,
  };
}

function render(props: Partial<React.ComponentProps<typeof ImportActivitySection>> = {}) {
  const defaults: React.ComponentProps<typeof ImportActivitySection> = {
    available: true,
    detail: undefined,
    wanted: [],
    queue: [],
    acquisitionState: 'worker',
    libraryScanState: 'live',
  };
  return renderToStaticMarkup(
    <MemoryRouter>
      <ImportActivitySection {...defaults} {...props} />
    </MemoryRouter>,
  );
}

// ── THE REQUIRED TEST: a null progress renders the seam text and NEVER a 0% bar ─────────────

describe('ImportActivitySection — the progress seam renders honestly', () => {
  it('a null-progress row renders "not tracked", never "0%"', () => {
    const html = render({ queue: [queueRow({ progress: null })] });
    expect(html).toContain('not tracked');
    expect(html).not.toContain('0%');
  });

  it('renders no percentage-shaped text at all for a null-progress row', () => {
    const html = render({ queue: [queueRow({ progress: null })] });
    // Guards against any numeric-percent rendering slipping in for the untracked case,
    // not just the specific "0%" string.
    expect(/\d+%/.test(html)).toBe(false);
  });

  it('a real numeric progress DOES render a percentage once the seam is closed', () => {
    const html = render({ queue: [queueRow({ progress: 37 })] });
    expect(html).toContain('37%');
  });
});

describe('ImportActivitySection — pipeline order and grouping', () => {
  it('renders group headers in queued -> downloading -> importing -> completed order', () => {
    const html = render({
      queue: [
        queueRow({ id: 1, status: 'completed', release_title: 'Completed.Release' }),
        queueRow({ id: 2, status: 'queued', release_title: 'Queued.Release' }),
        queueRow({ id: 3, status: 'importing', release_title: 'Importing.Release' }),
        queueRow({ id: 4, status: 'downloading', release_title: 'Downloading.Release' }),
      ],
    });
    const iQueued = html.indexOf('Queued.Release');
    const iDownloading = html.indexOf('Downloading.Release');
    const iImporting = html.indexOf('Importing.Release');
    const iCompleted = html.indexOf('Completed.Release');
    expect(iQueued).toBeGreaterThan(-1);
    expect(iQueued).toBeLessThan(iDownloading);
    expect(iDownloading).toBeLessThan(iImporting);
    expect(iImporting).toBeLessThan(iCompleted);
  });
});

describe('ImportActivitySection — degrade / empty / loading are visibly distinct', () => {
  it('loading (available null) shows a skeleton, no degrade card, no empty state', () => {
    const html = render({ available: null });
    expect(html).not.toContain('unavailable');
    expect(html).not.toContain('Nothing in the acquisition pipeline');
  });

  it('degraded (401) names CONSTELLATION_MUSE_TOKEN, not a bare "unavailable"', () => {
    const html = render({ available: false, detail: 'HTTP 401 for /api/requests/queue' });
    expect(html).toContain('CONSTELLATION_MUSE_TOKEN');
    expect(html).toContain('549');
  });

  it('empty (200, nothing queued) is visually distinct from degraded -- names acquisition wiring, not a generic empty', () => {
    const html = render({ available: true, queue: [], acquisitionState: 'unmounted' });
    expect(html).toContain('Nothing in the acquisition pipeline');
    expect(html).toContain('download client');
    expect(html).not.toContain('CONSTELLATION_MUSE_TOKEN');
  });

  it('empty because acquisition IS wired reports a different, neutral reason', () => {
    const html = render({ available: true, queue: [], acquisitionState: 'live' });
    expect(html).toContain('Nothing in the acquisition pipeline');
    expect(html).not.toContain('download client');
  });
});

describe('ImportActivitySection — wanted count links to the existing Requests panel, never duplicates it', () => {
  it('shows a compact count with no per-item rows rendered', () => {
    const html = render({ wanted: [wantedRow(), wantedRow({ media_metadata_id: 2, title: 'Another Wanted Title' })] });
    expect(html).toContain('2 waiting on a release');
    // The full row (e.g. its title) must NOT be duplicated into this section.
    expect(html).not.toContain('Another Wanted Title');
  });

  it('links to /muse/requests (MGUI-14)', () => {
    const html = render({ wanted: [wantedRow()] });
    expect(html).toContain('href="/muse/requests"');
  });

  it('names zero explicitly rather than a bare "0"', () => {
    const html = render({ wanted: [] });
    expect(html).toContain('Nothing waiting on a release');
  });
});

describe('ImportActivitySection — wiring chips reuse /api/subsystems, never invent a parallel state', () => {
  it('renders the acquisition and library_scan states verbatim', () => {
    const html = render({ acquisitionState: 'worker', libraryScanState: 'seam' });
    expect(html).toContain('worker');
    expect(html).toContain('seam');
  });

  it('renders "unknown" rather than guessing when a state is null', () => {
    const html = render({ acquisitionState: null, libraryScanState: null });
    expect(html).toContain('unknown');
  });
});
