// MGUI-19: the header line is a CLAIM about how much of the library you are looking at, and the
// previous one misled the operator into asking where 1600 titles had gone. Every case here is a
// sentence the page is allowed — or not allowed — to say.
import { describe, it, expect } from 'vitest';
import { librarySubtitle } from './LibraryPanel';

const base = {
  shown: 240,
  filtersActive: false,
  loaded: 240,
  total: 1892,
  onDisk: 1630,
  scoped: false,
  truncated: false,
};

describe('library subtitle honesty', () => {
  it('does not read as complete when only part of the library is loaded', () => {
    // THE REPORTED BUG. "240 of 240 loaded · 1892 in library" — every number true, and it reads
    // as a full fraction, so the operator reasonably asked where the other 1600 went.
    const s = librarySubtitle(base);
    expect(s).not.toContain('whole library');
    expect(s).toContain('240 of 240 loaded');
    expect(s).toContain('1892 in library');
  });

  it('says so plainly when the whole library IS loaded', () => {
    // The claim is only available when it is true, and it is stated rather than left to be
    // inferred by comparing two numbers.
    const s = librarySubtitle({ ...base, shown: 1892, loaded: 1892 });
    expect(s).toContain('your whole library');
  });

  it('warns when the page limit truncated the result', () => {
    // Without this, a capped page and a complete one differ only in two numbers the reader has
    // to compare. Hitting the cap means titles exist that are not on the page at all.
    const s = librarySubtitle({ ...base, loaded: 5000, truncated: true });
    expect(s).toContain('page limit reached');
  });

  it('never claims the whole library on a SCOPED page', () => {
    // The counts envelope is library-wide, so `total` on a Movies page is every title of both
    // kinds. Saying "your whole library" there would be false even when every movie is loaded,
    // and "of 1892" as this page's denominator would misdescribe what is missing.
    const s = librarySubtitle({ ...base, shown: 760, loaded: 760, scoped: true });
    expect(s).not.toContain('whole library');
    // The library-wide total is still shown, but labelled as spanning both kinds rather than
    // presented as this page's denominator.
    expect(s).toContain('1892 across all kinds');
  });

  it('marks a filtered count as matching, so it is not read as a total', () => {
    const s = librarySubtitle({ ...base, shown: 12, filtersActive: true });
    expect(s).toContain('12 matching of 240 loaded');
  });

  it('always reports the on-disk count', () => {
    expect(librarySubtitle(base)).toContain('1630 on disk');
  });
});
