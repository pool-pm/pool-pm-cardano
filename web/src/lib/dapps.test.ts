import { describe, it, expect } from 'vitest';
import { dappForAddress, dappForPolicy, isDex } from './dapps';

// Real mainnet addresses, all from the bundled CRFA snapshot unless noted.

/** Minswap's "Batch Order" script, exactly as the registry lists it. */
const MINSWAP_ORDER = 'addr1wyx22z2s4kasd3w976pnjf9xdty88epjqfvgkmfnscpd0rg3z8y6v';
/** Minswap's V2 order script, listed as a bare script address… */
const MINSWAP_V2_BARE = 'addr1wxn9efv2f6w82hagxqtn62ju4m293tqvw0uhmdl64ch8uwc0h43gt';
/** …and the same script hash deployed with Minswap's staking part. */
const MINSWAP_V2_STAKED =
  'addr1zxn9efv2f6w82hagxqtn62ju4m293tqvw0uhmdl64ch8uw6j2c79gy9l76sdg0xwhd7r0c0kna0tycz4y5s6mlenh8pq6s3z70';
/** The busiest script on mainnet: a Minswap contract the registry doesn't list at all,
 *  recognisable only by the staking part it shares with the ones it does. */
const MINSWAP_UNLISTED =
  'addr1z84q0denmyep98ph3tmzwsmw0j7zau9ljmsqx6a4rvaau66j2c79gy9l76sdg0xwhd7r0c0kna0tycz4y5s6mlenh8pq777e2a';
/** An ordinary wallet — no dApp behind it. */
const WALLET =
  'addr1qx2fxv2umyhttkxyxp8x0dlpdt3k6cwng5pxj3jhsydzer3jcu5d8ps7zex2k2xt3uqxgjqnnj83ws8lhrn648jjxtwq2ytjqp';
/** Minswap's Liquidity Provider Token policy (v1). */
const MINSWAP_LP_POLICY = 'e0baa1f0887a766daf5196f92c88728e356e71255c5ad00866607484';

describe('dappForAddress', () => {
  it('resolves an address the registry lists, with its role', () => {
    expect(dappForAddress(MINSWAP_ORDER)).toMatchObject({ name: 'Minswap', role: 'order', sub: 'AMM_DEX' });
  });

  it('resolves the same script deployed with a staking part', () => {
    // The registry lists the bare form; the staked form is the same script.
    expect(dappForAddress(MINSWAP_V2_BARE)?.role).toBe('order');
    expect(dappForAddress(MINSWAP_V2_STAKED)).toMatchObject({ name: 'Minswap', role: 'order' });
  });

  it('resolves an unlisted script by the staking part its dApp uses', () => {
    const dapp = dappForAddress(MINSWAP_UNLISTED)!;
    expect(dapp.name).toBe('Minswap');
    // The staking part says which project, never which contract — so no role is claimed.
    expect(dapp.role).toBeNull();
  });

  it('leaves an ordinary wallet unmatched', () => {
    expect(dappForAddress(WALLET)).toBeUndefined();
  });

  it('leaves a malformed address unmatched', () => {
    expect(dappForAddress('not-an-address')).toBeUndefined();
  });
});

describe('dappForPolicy', () => {
  it('names the dApp behind a known mint policy', () => {
    expect(dappForPolicy(MINSWAP_LP_POLICY)).toMatchObject({ name: 'Minswap' });
  });

  it('leaves an unknown policy unmatched', () => {
    expect(dappForPolicy('00'.repeat(28))).toBeUndefined();
  });
});

describe('isDex', () => {
  it('recognises every flavour of exchange', () => {
    expect(isDex({ name: 'x', category: 'DEFI', sub: 'AMM_DEX', role: null })).toBe(true);
    expect(isDex({ name: 'x', category: 'DEFI', sub: 'ORDERBOOK_DEX', role: null })).toBe(true);
    expect(isDex({ name: 'x', category: 'DEFI', sub: 'LENDING_BORROWING', role: null })).toBe(false);
    expect(isDex({ name: 'x', category: 'COLLECTION', sub: null, role: null })).toBe(false);
  });
});
