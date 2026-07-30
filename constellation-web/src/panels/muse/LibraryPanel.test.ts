// MGUI-01: unit coverage for the two rules that silently break this panel if they regress.
import { describe, it, expect } from 'vitest';
import { museArtUrl } from '../../hooks/useMuse';

describe('library poster art URLs', () => {
  // Regression for TERM #550: Muse's art resolver matches on ENTITY KIND and accepts only
  // `media_metadata` / `media_item`. Passing the VARIANT name (`poster`) silently yields a
  // placeholder for every tile, which is exactly how the on-deck rail shipped broken.
  it('uses an entity kind the Muse art resolver accepts, not a variant name', () => {
    const url = museArtUrl('media_metadata', '1225');
    expect(url).toBe('/api/muse/art/media_metadata/1225');
    expect(url).not.toContain('/art/poster/');
  });

  // The browser needs the same-origin proxy prefix; the API's own `poster_url`
  // (`/art/media_metadata/1225`) is Muse-relative and would 404 from the constellation origin.
  it('prefixes the constellation proxy path rather than using the Muse-relative path', () => {
    expect(museArtUrl('media_metadata', '7')).toMatch(/^\/api\/muse\/art\//);
  });

  it('encodes ids so a hostile id cannot escape the art path', () => {
    expect(museArtUrl('media_metadata', '../secret')).toBe('/api/muse/art/media_metadata/..%2Fsecret');
  });
});
