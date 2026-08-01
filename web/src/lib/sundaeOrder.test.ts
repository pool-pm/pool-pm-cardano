import { describe, it, expect } from 'vitest';
import { readSundaeSwapOrder } from './sundaeOrder';
import { isAda } from './dexOrder';

/**
 * A real SundaeSwap V3 order, taken verbatim from chain: 665.710078 ₳ offered for at
 * least 5,473,641,013 NIGHT. Unlike Minswap's, this datum names both assets outright —
 * `(policy, name, amount)` for each side — so nothing has to be resolved to read it.
 */
const ADA_FOR_NIGHT =
  'd8799fd8799f581c5b5d1f9da977498b5faf3efb83693b0442ed5f49d00d9b986a409c0bffd8799f581ce39b5f40aa85fbc121a625d777a776eca1cb4c923426949c997d8828ff1a00138800d8799fd8799fd8799f581c636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5affd8799fd8799fd8799f581ce39b5f40aa85fbc121a625d777a776eca1cb4c923426949c997d8828ffffffffd87980ffd87a9f9f40401a27adedfeff9f581c0691b2fecca1ac4f53cb6dfb00b7013e561d1f34403b957cbb5af1fa454e494748541b0000000146412235ffff43d87980ff';

const NIGHT_POLICY = '0691b2fecca1ac4f53cb6dfb00b7013e561d1f34403b957cbb5af1fa';
/** "NIGHT" as the asset name. */
const NIGHT_NAME = '4e49474854';

describe('readSundaeSwapOrder', () => {
  const order = readSundaeSwapOrder(ADA_FOR_NIGHT)!;

  it('names both sides from the datum alone', () => {
    expect(isAda(order.give)).toBe(true);
    expect(order.want).toEqual({ policy: NIGHT_POLICY, name: NIGHT_NAME });
  });

  it('reads the offered amount exactly', () => {
    // From the datum, not the order UTXO — which also holds the 1.28 ₳ protocol fee.
    expect(order.giveAmount).toBe(665710078n);
  });

  it('has the sides the right way round', () => {
    // NIGHT trades well under 1 ₳, so an ADA offer buys far more NIGHT than the ₳ it
    // costs. Reversed, this would read as paying 665 million NIGHT for 5,439 ₳.
    expect(Number(order.giveAmount)).toBeLessThan(5473641013);
  });
});

describe('readSundaeSwapOrder: what it declines', () => {
  it('has nothing to say about a datum it cannot read', () => {
    expect(readSundaeSwapOrder('deadbeef')).toBeNull();
    expect(readSundaeSwapOrder(undefined)).toBeNull();
  });

  it('declines an order that is not a swap', () => {
    // Move `details` off Order.Swap (ctor 1, d87a9f) to Deposit (ctor 2, d87b9f).
    const deposit = ADA_FOR_NIGHT.replace('ffd87a9f9f4040', 'ffd87b9f9f4040');
    expect(deposit).not.toBe(ADA_FOR_NIGHT);
    expect(readSundaeSwapOrder(deposit)).toBeNull();
  });
});
