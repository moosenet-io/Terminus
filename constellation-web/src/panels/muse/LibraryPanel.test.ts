// MGUI-01/15: the rules that silently break this panel if they regress.
import { describe, it, expect } from 'vitest';
import { museArtUrl, museArtUrlAt } from '../../hooks/useMuse';
import { alphaKey } from './LibraryPanel';

describe('library poster art URLs', () => {
  // Regression for TERM #550: Muse's art resolver matches on ENTITY KIND and accepts only
  // `media_metadata` / `media_item`. Passing the VARIANT name (`poster`) silently yields a
  // placeholder for every tile, which is exactly how the on-deck rail shipped broken.
  it('uses an entity kind the Muse art resolver accepts, not a variant name', () => {
    expect(museArtUrl('media_metadata', '1225')).toBe('/api/muse/art/media_metadata/1225');
    expect(museArtUrl('media_metadata', '1225')).not.toContain('/art/poster/');
  });

  it('prefixes the constellation proxy path rather than using the Muse-relative path', () => {
    expect(museArtUrl('media_metadata', '7')).toMatch(/^\/api\/muse\/art\//);
  });

  it('encodes ids so a hostile id cannot escape the art path', () => {
    expect(museArtUrl('media_metadata', '../secret')).toBe('/api/muse/art/media_metadata/..%2Fsecret');
  });

  // MGUI-15 / MUSE #100: the grid MUST request a rendition. Without ?w= the endpoint
  // serves the full-size master — 1.9MB for one ~112px tile.
  it('requests a ladder width for thumbnails', () => {
    expect(museArtUrlAt('media_metadata', '1225', 160)).toBe('/api/muse/art/media_metadata/1225?w=160');
    expect(museArtUrlAt('media_metadata', '1225', 640)).toBe('/api/muse/art/media_metadata/1225?w=640');
  });
});

describe('alphabet index', () => {
  // Leading articles are stripped so "The Martian" lands under M — where someone
  // looking for it will press. This is how Plex/Jellyfin index a library, and the
  // A→Z sort uses the same transform so the rail and the order cannot disagree.
  it('files a title under its first significant letter, not its article', () => {
    expect(alphaKey('The Martian')).toBe('M');
    expect(alphaKey('A Walk in the Clouds')).toBe('W');
    expect(alphaKey('An Education')).toBe('E');
    expect(alphaKey('Silo')).toBe('S');
  });

  it('is case- and whitespace-insensitive', () => {
    expect(alphaKey('  the martian')).toBe('M');
    expect(alphaKey('sILO')).toBe('S');
  });

  // Anything not starting with a letter buckets under '#' rather than creating a
  // phantom rail entry per symbol.
  it('buckets non-letters under #', () => {
    expect(alphaKey('10 Things I Hate About You')).toBe('#');
    expect(alphaKey('¡Three Amigos!')).toBe('#');
    expect(alphaKey('')).toBe('#');
  });

  // "The" alone must not strip to nothing and crash the bucket lookup.
  it('does not strip a bare article into an empty key', () => {
    expect(alphaKey('The')).toBe('T');
    expect(alphaKey('A')).toBe('A');
  });
});
