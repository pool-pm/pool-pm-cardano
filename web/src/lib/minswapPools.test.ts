import { describe, it, expect } from 'vitest';
import { isAda, poolPair } from './minswapPools';
import table from './minswap-pools.json';

/** Minswap V2's LP policy. */
const V2 = 'f5808c2c990d86da54bfc97d89cee6efa20cd8461616359478d96b4c';
/** The ADA/MIN V2 pool, taken from Minswap's own API. */
const ADA_MIN = '82e2b1fd27a7712a1a9cf750dfbea1a5778611b20e06dd6a611df7a643f8cb75';
const MIN_POLICY = '29d222ce763455e3d7a09a665ce554f00ac89d2e99a1a83d267170c6';

describe('poolPair', () => {
  it('resolves a pool to its pair, in hash order', () => {
    const pair = poolPair(V2, ADA_MIN)!;
    expect(isAda(pair[0])).toBe(true);
    expect(pair[1]).toEqual({ policy: MIN_POLICY, name: '4d494e' });
  });

  it('refuses a name minted under another policy', () => {
    // V1's LP policy. Its names derive differently, so answering would be a fabrication.
    expect(poolPair('e4214b7cce62ac6fbba385d164df48e157eae5863521b4b67ca71d86', ADA_MIN)).toBeUndefined();
  });

  it('has no answer for a pool outside the table', () => {
    expect(poolPair(V2, 'ff'.repeat(32))).toBeUndefined();
  });
});

describe('the bundled table', () => {
  it('holds a pair for every pool, with no dangling asset index', () => {
    const { assets, pools } = table as { assets: string[][]; pools: Record<string, number[]> };
    expect(Object.keys(pools).length).toBeGreaterThan(100);
    for (const [name, pair] of Object.entries(pools)) {
      expect(name).toMatch(/^[0-9a-f]{64}$/);
      expect(pair).toHaveLength(2);
      for (const i of pair) expect(assets[i]).toBeDefined();
    }
  });

  // The pairs themselves are verified where the hash is available: the generator
  // recomputes sha3_256(sha3_256(A) ++ sha3_256(B)) for every entry and refuses to write
  // one that doesn't reproduce its own LP name. Repeating it here would need node:crypto,
  // which this suite can't import without pulling @types/node into the app's typecheck.
});
