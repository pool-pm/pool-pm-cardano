import { describe, it, expect } from 'vitest';
import { commonNamePrefix } from './assetName';

describe('commonNamePrefix', () => {
  it('derives a collection name from "#"-indexed names', () => {
    expect(commonNamePrefix(['Clay Nation #4821', 'Clay Nation #12', 'Clay Nation #7'])).toBe('Clay Nation');
  });

  it('handles a space-separated index', () => {
    expect(commonNamePrefix(['SpaceBudz 1029', 'SpaceBudz 2000'])).toBe('SpaceBudz');
  });

  it('keeps a shared name with no numbering', () => {
    expect(commonNamePrefix(['Deadpxlz', 'Deadpxlz'])).toBe('Deadpxlz');
  });

  it('stops at the divergence point mid-token', () => {
    // LCP is "Ada" then diverges; no trailing digits/separators to strip.
    expect(commonNamePrefix(['Adapunk', 'Adaracer'])).toBe('Ada');
  });

  it('ignores missing names', () => {
    expect(commonNamePrefix([undefined, 'Boss Cat #1', 'Boss Cat #2', undefined])).toBe('Boss Cat');
  });

  it('returns empty when names share no prefix', () => {
    expect(commonNamePrefix(['Alpha', 'Beta'])).toBe('');
  });

  it('returns empty for no valid names', () => {
    expect(commonNamePrefix([undefined, undefined])).toBe('');
    expect(commonNamePrefix([])).toBe('');
  });
});
