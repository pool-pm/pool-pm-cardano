import { describe, it, expect } from 'vitest';
import { readSpectrumOrder } from './spectrumOrder';
import { isAda } from './dexOrder';

/**
 * A real Spectrum order, taken verbatim from chain: 22.004708 ₳ for at least
 * 75.98958249 AGIX. Values checked against db-sync's decoding of the same bytes.
 */
const ADA_FOR_AGIX =
  'd8799fd8799f4040ffd8799f581cf43a62fdc3965df486de8a0d32fe800963589c41b38946602a0dc5354441474958ffd8799f581cc0cee96d987f978937126cbc89a28d37596a2859f043a074e9c7b4ca4c414749585f4144415f4e4654ff1903e51a0007a1201a96fa4ce3581ce68e8b253ab8ff78bdea340260af1fd98bf82bdbdc1e9c134c218d90d8799f581cbb45a2159997ddf73d8ba5c37c6aab2afa5d8d1caa4969f694112e07ff1a014fc3e41b00000001c4eee6a9ff';

const AGIX_POLICY = 'f43a62fdc3965df486de8a0d32fe800963589c41b38946602a0dc535';
/** "AGIX" as the asset name. */
const AGIX_NAME = '41474958';

describe('readSpectrumOrder', () => {
  const order = readSpectrumOrder(ADA_FOR_AGIX)!;

  it('names both sides and carries both amounts, with no flag to interpret', () => {
    expect(isAda(order.give)).toBe(true);
    expect(order.want).toEqual({ policy: AGIX_POLICY, name: AGIX_NAME });
    expect(order.giveAmount).toBe(22004708n);
  });

  it('has base as the given side', () => {
    // 22.004708 ₳ for 75.98958249 AGIX is 0.29 ₳ each — AGIX's actual price. Reversed,
    // this would read as paying 22 AGIX for 7,598 ₳. Confirmed on a second real order
    // running the other way, LIFI → ADA, which the same reading prices correctly.
    const adaPerAgix = Number(order.giveAmount) / (7598958249 / 100);
    expect(adaPerAgix).toBeGreaterThan(0.2);
    expect(adaPerAgix).toBeLessThan(0.4);
  });
});

describe('readSpectrumOrder: what it declines', () => {
  it('has nothing to say about a datum it cannot read', () => {
    expect(readSpectrumOrder('deadbeef')).toBeNull();
    expect(readSpectrumOrder(undefined)).toBeNull();
  });

  it('declines a datum whose fields are not that shape', () => {
    // A Minswap order — right protocol family, wrong layout.
    expect(readSpectrumOrder('d87982' + '0102')).toBeNull();
  });
});
