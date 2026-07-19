// CONST-23: deep-link query-string codec for the MINT global filter row (§7.1). Kept separate
// from useMint.ts's filtersToQuery (that one shapes the *backend request* query — comma-joined
// `models=`; this one shapes the *browser URL* — repeated `model=` params, the more idiomatic
// deep-link convention and independent of how the mock/real endpoint wants it).
import type { MintFilters } from '../../hooks/useMint';
import { DEFAULT_MINT_FILTERS } from '../../hooks/useMint';

const TASK_CATEGORIES = new Set(['all', 'blitz', 'multi_file', 'deep']);
const BACKEND_TAGS = new Set(['all', 'gpu', 'cpu']);

/** Model multi-select ceiling (§7.1: "model multi-select (<=4)"). */
export const MINT_MODEL_SELECT_CAP = 4;

export function parseMintFilters(params: URLSearchParams): MintFilters {
  const epoch = params.get('epoch') ?? DEFAULT_MINT_FILTERS.epoch;
  const taskCategoryRaw = params.get('task_category') ?? 'all';
  const backendTagRaw = params.get('backend_tag') ?? 'all';
  const models = params.getAll('model').filter(Boolean).slice(0, MINT_MODEL_SELECT_CAP);

  return {
    epoch,
    taskCategory: TASK_CATEGORIES.has(taskCategoryRaw) ? (taskCategoryRaw as MintFilters['taskCategory']) : 'all',
    backendTag: BACKEND_TAGS.has(backendTagRaw) ? (backendTagRaw as MintFilters['backendTag']) : 'all',
    models,
  };
}

/** Writes `filters` into a fresh URLSearchParams, omitting anything at its default so links
 *  stay short (deep link restores exactly, since parse fills in the same defaults). */
export function mintFiltersToParams(filters: MintFilters): URLSearchParams {
  const params = new URLSearchParams();
  if (filters.epoch !== DEFAULT_MINT_FILTERS.epoch) params.set('epoch', filters.epoch);
  if (filters.taskCategory !== 'all') params.set('task_category', filters.taskCategory);
  if (filters.backendTag !== 'all') params.set('backend_tag', filters.backendTag);
  for (const m of filters.models.slice(0, MINT_MODEL_SELECT_CAP)) params.append('model', m);
  return params;
}
