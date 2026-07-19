// CONST-23: /mint — one page, sectioned (§7.1), sticky in-page section nav, ONE global filter
// row scoping every section. Filters are deep-linked in the query string (parse on mount,
// re-serialize on every change via replaceState-style navigate so back/forward doesn't spam
// history for every filter tweak) — a bookmarked/shared MINT URL restores the exact view.
import { useCallback, useMemo } from 'react';
import { useSearchParams } from 'react-router-dom';
import { MintFilterBar } from './MintFilterBar';
import { OverviewSection } from './OverviewSection';
import { CoverageSection } from './CoverageSection';
import { CapabilitySection } from './CapabilitySection';
import { ContextSection } from './ContextSection';
import { CoderSection } from './CoderSection';
import { parseMintFilters, mintFiltersToParams } from './mintFilters';
import type { MintFilters } from '../../hooks/useMint';

const SECTIONS = [
  { id: 'overview', label: 'Overview' },
  { id: 'coverage', label: 'Coverage' },
  { id: 'capability', label: 'Capability' },
  { id: 'coder', label: 'Coder' },
  { id: 'context', label: 'Context' },
] as const;

const EPOCH_OPTIONS = [
  { value: 'current', label: 'Current (S119)' },
  { value: 'S118', label: 'S118' },
  { value: 'S110', label: 'S110 (sparse fixture)' },
  { value: 'all', label: 'All epochs' },
];

export function MintPage() {
  const [searchParams, setSearchParams] = useSearchParams();

  const filters: MintFilters = useMemo(() => parseMintFilters(searchParams), [searchParams]);

  const handleFiltersChange = useCallback((next: MintFilters) => {
    setSearchParams(mintFiltersToParams(next), { replace: true });
  }, [setSearchParams]);

  return (
    <div style={{ height: '100%', overflowY: 'auto', display: 'flex', flexDirection: 'column' }}>
      <MintFilterBar filters={filters} onChange={handleFiltersChange} epochOptions={EPOCH_OPTIONS} />

      <nav
        aria-label="MINT sections"
        style={{
          display: 'flex', gap: 'var(--space-4)', padding: 'var(--space-2) var(--space-4)',
          borderBottom: '1px solid var(--border)', background: 'var(--bg-page)',
          position: 'sticky', top: 53, zIndex: 4, overflowX: 'auto',
        }}
      >
        {SECTIONS.map(s => (
          <a
            key={s.id}
            href={`#${s.id}`}
            style={{
              fontFamily: 'var(--font-mono)', fontSize: 'var(--fs-mono-sm)', textTransform: 'uppercase',
              letterSpacing: 'var(--ls-label)', color: 'var(--text-300)', textDecoration: 'none',
              whiteSpace: 'nowrap', padding: '4px 0',
            }}
          >
            {s.label}
          </a>
        ))}
      </nav>

      <div style={{ padding: 'var(--space-4)', display: 'flex', flexDirection: 'column', gap: 'var(--space-6)' }}>
        <OverviewSection filters={filters} />
        <CoverageSection filters={filters} onFiltersChange={handleFiltersChange} />
        <CapabilitySection filters={filters} />
        <CoderSection filters={filters} onFiltersChange={handleFiltersChange} />
        <ContextSection filters={filters} />
      </div>
    </div>
  );
}
