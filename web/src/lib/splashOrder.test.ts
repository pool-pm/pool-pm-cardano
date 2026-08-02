import { describe, it, expect } from 'vitest';
import { readSplashOrder } from './splashOrder';

// A real mainnet Splash order: 100 ₳ in, SONGMARKETCAP wanted back. The UTXO holding it
// carried 105 ₳ — the extra 5 ₳ being the protocol's fees and a deposit that comes back,
// which is why the amount is taken from the datum and not from the UTXO.
const ADA_FOR_SONGMARKETCAP =
  'd8799f4100581c897115e5a91480a1c235f99999564871b3aed93a4f4297f2214b1032d8799f4040ff1a05f5e1001a000f4240190d78d8799f581cf71b4cf652d8edb33a57928b8b8a546a3c954b7ba24db5583ac79b344d534f4e474d41524b4554434150ffd8799f190d781a05f5e100ff1a001e8480d8799fd8799f581c557df02f7cfbf1a65ca15bde8ae89e105e2e0bf4a76ab14ed4fb3efaffd8799fd8799fd8799f581cd08e6b9ce991728dd9336b117d4417ae7b170e2b84f08a075f0bc89effffffff581c557df02f7cfbf1a65ca15bde8ae89e105e2e0bf4a76ab14ed4fb3efa9f581c5cb2c968e5d1c7197a6ce7615967310a375545d9bc65063a964335b2ffff';

describe('readSplashOrder', () => {
  it('reads both sides and the exact amount going in', () => {
    const order = readSplashOrder(ADA_FOR_SONGMARKETCAP)!;
    expect(order.give).toEqual({ policy: '', name: '' });
    expect(order.giveAmount).toBe(100_000_000n);
    expect(order.want).toEqual({
      policy: 'f71b4cf652d8edb33a57928b8b8a546a3c954b7ba24db5583ac79b34',
      name: '534f4e474d41524b4554434150',
    });
  });

  it('returns null rather than throwing on anything that isn’t one', () => {
    expect(readSplashOrder(undefined)).toBeNull();
    expect(readSplashOrder('')).toBeNull();
    expect(readSplashOrder('zz')).toBeNull();
    // A well-formed datum of the wrong shape: a constructor with too few fields.
    expect(readSplashOrder('d87980')).toBeNull();
  });
});
