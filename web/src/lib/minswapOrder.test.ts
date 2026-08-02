import { describe, it, expect } from 'vitest';
import { readSwapOrder } from './minswapOrder';
import { isAda } from './dexOrder';

/**
 * A real Minswap V2 order, taken verbatim from chain. Its pool is ADA / WorldMobileTokenX
 * and its direction flag is `Constr 1` — `A_TO_B` in Minswap's SDK, so ADA in, WMTX out.
 */
const ADA_FOR_WMTX =
  'd8799fd8799f581c636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5affd8799fd8799f581c636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5affd8799fd8799fd8799f581ce39b5f40aa85fbc121a625d777a776eca1cb4c923426949c997d8828ffffffffd87980d8799fd8799f581c636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5affd8799fd8799fd8799f581ce39b5f40aa85fbc121a625d777a776eca1cb4c923426949c997d8828ffffffffd87980d8799f581cf5808c2c990d86da54bfc97d89cee6efa20cd8461616359478d96b4c5820686db0c143a3a2cc19099d8909e315c4ed761a6ac5a3c5998c651d5e9d3cb253ffd8799fd87a80d8799f1a26ef03a4ff1adfaf40f4d87980ff1a001e8480d87a80ff';

const WMTX_POLICY = 'e5a42a1a1d3d1da71b0449663c32798725888d2eb0843c4dabeca05a';

const USDM_FOR_STRIKE_ROUTED =
  'd8799fd8799f581c9abc0b287a401428b9e2205b439d834e029d3802b95155980d34be31ffd8799fd8799f581c9abc0b287a401428b9e2205b439d834e029d3802b95155980d34be31ffd8799fd8799fd8799f581c0f1dbb5c72409275b686faf739ba9df6682017e1337aa292b7900547ffffffffd87980d8799fd8799f581c9abc0b287a401428b9e2205b439d834e029d3802b95155980d34be31ffd8799fd8799fd8799f581c0f1dbb5c72409275b686faf739ba9df6682017e1337aa292b7900547ffffffffd87980d8799f581cf5808c2c990d86da54bfc97d89cee6efa20cd8461616359478d96b4c58207dd6988c5a86693c76aeec1ea94afa41770be0de21a775ca7a2a1eabdb6a0171ffd905029f82d8799fd8799f581cf5808c2c990d86da54bfc97d89cee6efa20cd8461616359478d96b4c58207dd6988c5a86693c76aeec1ea94afa41770be0de21a775ca7a2a1eabdb6a0171ffd87980ffd8799fd8799f581cf5808c2c990d86da54bfc97d89cee6efa20cd8461616359478d96b4c582073e1518e92f367fd5820ac2da1d40ab24fbca1d6cb2c28121ad92f57aff8abceffd87a80ffd8799f1a01bd722cff1a0403741bff1a001e8480d87a80ff';

describe('readSwapOrder', () => {
  const order = readSwapOrder(ADA_FOR_WMTX)!;

  it('reads which asset goes each way', () => {
    expect(isAda(order.give)).toBe(true);
    expect(order.want.policy).toBe(WMTX_POLICY);
  });

  it('takes the amount from the datum, not the order UTXO', () => {
    // The UTXO also holds the 2 ₳ batcher fee and a deposit that comes back, so reading
    // it would overstate the swap by several ₳.
    expect(order.giveAmount).toBe(653198244n);
  });

  it('reads the direction the way round the SDK defines it', () => {
    // `Direction { B_TO_A = 0, A_TO_B = 1 }` — the reverse of the obvious guess, and
    // getting it backwards inverts every swap while still looking plausible.
    //
    // Cross-checked against the market: this prices WMTX at 653.198244 / 3752.804596 =
    // 0.174 ₳ each. A SundaeSwap order for the same token in the same period priced it at
    // 708.365189 / 4128.085056 = 0.172 ₳. Reversed, this would read 5.7 WMTX per ₳ —
    // off by a factor of 33 from an independent venue.
    const adaPerWmtx = Number(order.giveAmount) / 3752804596;
    expect(adaPerWmtx).toBeGreaterThan(0.15);
    expect(adaPerWmtx).toBeLessThan(0.2);
  });
});

describe('readSwapOrder: what it declines', () => {
  it('has nothing to say about a datum it cannot read', () => {
    expect(readSwapOrder('deadbeef')).toBeNull();
    expect(readSwapOrder(undefined)).toBeNull();
  });

  it('declines a step that is not a plain swap', () => {
    // Same datum with the step's constructor moved off SWAP_EXACT_IN (d8799f -> d87a9f,
    // tag 121 -> 122), which is a STOP order: a different shape, not a two-sided swap.
    const stop = ADA_FOR_WMTX.replace('ffd8799fd87a80d8799f1a26ef03a4ff', 'ffd87a9fd87a80d8799f1a26ef03a4ff');
    expect(stop).not.toBe(ADA_FOR_WMTX);
    expect(readSwapOrder(stop)).toBeNull();
  });

  it('declines a pool outside the bundled table', () => {
    // Same datum with an LP name that resolves to nothing — inventing a pair would be
    // worse than saying nothing.
    const unknown = ADA_FOR_WMTX.replace(
      '686db0c143a3a2cc19099d8909e315c4ed761a6ac5a3c5998c651d5e9d3cb253',
      'ff'.repeat(32),
    );
    expect(unknown).not.toBe(ADA_FOR_WMTX);
    expect(readSwapOrder(unknown)).toBeNull();
  });

  it('reads a swap routed through several pools', () => {
    // A real mainnet order: 29.192748 USDM in, STRIKE out, hopping through ADA because
    // the pair has no pool of its own. Only the ends of the chain are the swap — the
    // intermediate asset is a mechanism, not something the user asked for.
    const order = readSwapOrder(USDM_FOR_STRIKE_ROUTED)!;
    expect(order.giveAmount).toBe(29_192_748n);
    // CIP-67 labelled (0014df10) USDM in, STRIKE out.
    expect(order.give.name).toBe('0014df105553444d');
    expect(order.want.name).toBe('535452494b45');
  });

  it('declines a route whose far pool is outside the bundled table', () => {
    // The last hop names the wanted asset, so an unresolved pool there leaves the swap's
    // own end unnamed — better to say nothing than to report only where it started.
    const unknownFarPool = USDM_FOR_STRIKE_ROUTED.replace(
      '73e1518e92f367fd5820ac2da1d40ab24fbca1d6cb2c28121ad92f57aff8abce',
      'ff'.repeat(32),
    );
    expect(unknownFarPool).not.toBe(USDM_FOR_STRIKE_ROUTED);
    expect(readSwapOrder(unknownFarPool)).toBeNull();
  });
});
