// The delegators grid's sort control cycles through four states on click. The button's
// arrow points down at rest and turns a quarter clockwise per click — down, left, up,
// right — which is exactly this order (CSS `rotate()` is clockwise in screen coordinates,
// so a down arrow at +90° points left).
export type SortState = 'stake-desc' | 'time-desc' | 'stake-asc' | 'time-asc';

export const SORT_CYCLE: readonly SortState[] = ['stake-desc', 'time-desc', 'stake-asc', 'time-asc'];

export function nextSort(state: SortState): SortState {
  return SORT_CYCLE[(SORT_CYCLE.indexOf(state) + 1) % SORT_CYCLE.length];
}

/** Quarter-turns of the arrow for a state — its index in the cycle. */
export function sortIndex(state: SortState): number {
  const i = SORT_CYCLE.indexOf(state);
  return i < 0 ? 0 : i;
}

/** Server query params. The default state sends nothing, so its URL stays clean. */
export function sortParams(state: SortState): { sort?: 'time'; order?: 'asc' } {
  return {
    ...(state.startsWith('time') ? { sort: 'time' as const } : {}),
    ...(state.endsWith('asc') ? { order: 'asc' as const } : {}),
  };
}

/** Read the state back from the page URL (so Back restores the sort). */
export function sortFromParams(params: URLSearchParams): SortState {
  const axis = params.get('sort') === 'time' ? 'time' : 'stake';
  const dir = params.get('order') === 'asc' ? 'asc' : 'desc';
  return `${axis}-${dir}` as SortState;
}

export function sortTitle(state: SortState): string {
  switch (state) {
    case 'stake-desc':
      return 'Biggest stake first — click to sort by newest delegation';
    case 'time-desc':
      return 'Newest delegation first — click to sort by smallest stake';
    case 'stake-asc':
      return 'Smallest stake first — click to sort by oldest delegation';
    case 'time-asc':
      return 'Oldest delegation first — click to sort by biggest stake';
  }
}
