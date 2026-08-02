import { describe, it, expect } from 'vitest';
import { readCSwapOrder } from './cswapOrder';

// A real mainnet CSwap order. The datum records only the bundle the order is to be paid;
// what's being handed over is whatever the order UTXO holds — here 81,592 SNEK.
const SNEK_FOR_ADA =
  'd8799fd8799fd8799f581c15272ff9aff3eca612c7305f70ff52951368e80cd2325f9b4765a3e5ffd8799fd8799fd8799f581ccd1a644f4672092569a1e46516034325ed004e51b73f8c94e39221e3ffffffff9f9f40401a0851e25fffff9f9f581c279c909f348e533da5808898f87f9a14bb2c3dfbbacccd631d927a3f44534e454b00ffffd87980000fff';

const UTXO = {
  lovelace: '2690000',
  assets: [{ name: 'SNEK', quantity: '81592' }],
};

describe('readCSwapOrder', () => {
  it('takes the wanted side from the datum and the given side from the UTXO', () => {
    const order = readCSwapOrder(SNEK_FOR_ADA, UTXO)!;
    expect(order).not.toBeNull();
    // Giving a token, so the amount comes from the UTXO rather than the datum.
    expect(order.give.policy).not.toBe('');
    expect(order.giveAmount).toBeUndefined();
  });

  it('needs the UTXO, since the datum alone doesn’t say what is being given', () => {
    expect(readCSwapOrder(SNEK_FOR_ADA, undefined)).toBeNull();
  });

  it('returns null rather than throwing on anything that isn’t one', () => {
    expect(readCSwapOrder(undefined, UTXO)).toBeNull();
    expect(readCSwapOrder('zz', UTXO)).toBeNull();
    expect(readCSwapOrder('d87980', UTXO)).toBeNull();
  });
});
