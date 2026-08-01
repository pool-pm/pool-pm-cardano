import { describe, it, expect } from 'vitest';
import type { AssetInfo, TxInput, TxOutputInfo } from './types';
import { readSettlement } from './settlement';
import { stakeAddressOf } from './bech32';

// A real Minswap settlement, reproduced from chain: one user swapping NIGHT for ADA.
// The pool gained 42,126.614528 NIGHT and lost 4,452.204671 ₳ — which is the swap,
// exactly. The order UTXO's own 4 ₳ is batcher fee and deposit, not part of it.

/** Minswap's V2 pool script (role `pool`). */
const POOL = 'addr1z84q0denmyep98ph3tmzwsmw0j7zau9ljmsqx6a4rvaau66j2c79gy9l76sdg0xwhd7r0c0kna0tycz4y5s6mlenh8pq777e2a';
/** Minswap's V2 order script (role `order`). */
const ORDER = 'addr1w8p79rpkcdz8x9d6tft0x0dx5mwuzac2sa4gm8cvkw5hcnqst2ctf';
/** The batcher: funds the tx and takes its change back, so it appears on both sides. */
const BATCHER =
  'addr1q9l642m4y7smwuj3e57e2xxa6pt6g3wrk7dyvh9960ezxnrcq7dmqu20hxxtcts5zkz7jaqrwua8claa2hrghvgnwnpq458vet';
/** The user being paid — absent from the inputs, having posted their order earlier. */
const USER = 'addr1qx2fxv2umyhttkxyxp8x0dlpdt3k6cwng5pxj3jhsydzer3jcu5d8ps7zex2k2xt3uqxgjqnnj83ws8lhrn648jjxtwq2ytjqp';

const NIGHT = 'asset1night';

function night(quantity: string): AssetInfo {
  return { fingerprint: NIGHT, name: 'NIGHT', quantity, size: 256 };
}

function input(address: string, lovelace: string, assets: AssetInfo[] = []): TxInput {
  return { tx_hash: '00'.repeat(32), index: 0, address, lovelace, assets };
}

function output(address: string, lovelace: string, assets: AssetInfo[] = []): TxOutputInfo {
  return { address, lovelace, assets };
}

const accountOf = (address: string) => stakeAddressOf(address) ?? address;

/** The settlement above, with the pool's two sides parameterised for the edge cases. */
function settlement(overrides: { inputs?: TxInput[]; outputs?: TxOutputInfo[] } = {}) {
  return readSettlement(
    overrides.inputs ?? [
      input(POOL, '2109561210296', [night('19858645.356383')]),
      input(BATCHER, '275608917'),
      input(ORDER, '4000000', [night('42126.614528')]),
    ],
    overrides.outputs ?? [
      output(BATCHER, '276942964'),
      output(USER, '4454204671'),
      output(POOL, '2105109005625', [night('19900771.970911')]),
    ],
    accountOf,
  );
}

describe('readSettlement', () => {
  it('reads both sides of the swap from the pool balance change', () => {
    const settled = settlement()!;
    // What the pool gained is what the user gave.
    expect(settled.gave).toMatchObject({ quantity: '42126.614528' });
    expect(settled.gave.asset?.name).toBe('NIGHT');
    // What the pool lost is what the user got — not the 4,454.2 ₳ output, which also
    // returns the order's deposit.
    expect(settled.got).toEqual({ quantity: '4452204671' });
  });

  it('identifies the user as the output that funded nothing', () => {
    expect(settlement()!.beneficiary.address).toBe(USER);
  });

  it('gives up when several orders share one pool movement', () => {
    const settled = settlement({
      inputs: [
        input(POOL, '2109561210296', [night('19858645.356383')]),
        input(BATCHER, '275608917'),
        input(ORDER, '4000000', [night('42126.614528')]),
        input(ORDER, '4000000', [night('1000.0')]),
      ],
    });
    // The pool delta is the sum of both; splitting it between them would be a guess.
    expect(settled).toBeNull();
  });

  it('gives up when the payout is ambiguous', () => {
    const settled = settlement({
      outputs: [
        output(BATCHER, '276942964'),
        output(USER, '4454204671'),
        output('addr1v9nmg2xhpqx5j7ky4ldpx6dngv3q9r2t2ymxjr6kd4y5r3q7ry0z6', '1000000'),
        output(POOL, '2105109005625', [night('19900771.970911')]),
      ],
    });
    expect(settled).toBeNull();
  });

  it('gives up on a deposit, where the pool gains both sides', () => {
    const settled = settlement({
      outputs: [
        output(BATCHER, '276942964'),
        output(USER, '4454204671'),
        // Pool up on ADA and up on NIGHT: adding liquidity, not swapping.
        output(POOL, '2119561210296', [night('19900771.970911')]),
      ],
    });
    expect(settled).toBeNull();
  });
});
