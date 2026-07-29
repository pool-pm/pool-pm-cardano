import { describe, it, expect } from 'vitest';
import { nextSort, sortFromParams, sortIndex, sortParams, SORT_CYCLE, type SortState } from './sortCycle';

describe('sort cycle', () => {
  it('cycles down → left → up → right and wraps', () => {
    let s: SortState = 'stake-desc';
    const seen: SortState[] = [s];
    for (let i = 0; i < 4; i++) seen.push((s = nextSort(s)));
    expect(seen).toEqual(['stake-desc', 'time-desc', 'stake-asc', 'time-asc', 'stake-desc']);
  });

  it('gives each state its quarter-turn index', () => {
    expect(SORT_CYCLE.map(sortIndex)).toEqual([0, 1, 2, 3]);
  });

  it('sends nothing for the default state, and the right pair otherwise', () => {
    expect(sortParams('stake-desc')).toEqual({});
    expect(sortParams('time-desc')).toEqual({ sort: 'time' });
    expect(sortParams('stake-asc')).toEqual({ order: 'asc' });
    expect(sortParams('time-asc')).toEqual({ sort: 'time', order: 'asc' });
  });

  it('round-trips through URL params, and falls back to the default on junk', () => {
    for (const state of SORT_CYCLE) {
      const params = new URLSearchParams(sortParams(state) as Record<string, string>);
      expect(sortFromParams(params)).toBe(state);
    }
    expect(sortFromParams(new URLSearchParams(''))).toBe('stake-desc');
    expect(sortFromParams(new URLSearchParams('sort=nope&order=sideways'))).toBe('stake-desc');
  });
});
