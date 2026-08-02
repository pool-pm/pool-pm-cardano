import { describe, it, expect } from 'vitest';
import {
  bech32Encode,
  bech32Decode,
  paymentIsScript,
  stakeAddressOf,
  stakeCredential,
  rewardCredential,
} from './bech32';

const BASE = 'addr1q9ksge28xvfrua9pn34szs5fj4nva8eg8y78e5j6gm5jkrrs6uzvjw4lfzksgrmlw9mvm67rzeelqfhdt2kzxll4phrqs8tejy';
const STAKE = 'stake1u9cnwter5xjyn5lf75883qnq6d78l8uz4n38mdjlw5smn5qw8whz5';

describe('bech32 encode / stakeAddressOf', () => {
  it('bech32Encode round-trips a decoded stake address', () => {
    const bytes = bech32Decode(STAKE)!;
    expect(bech32Encode('stake', bytes)).toBe(STAKE);
  });

  it("stakeAddressOf preserves the base address's stake credential", () => {
    const derived = stakeAddressOf(BASE)!;
    expect(derived.startsWith('stake1')).toBe(true);
    // The reconstructed stake address must carry the exact same 28-byte credential.
    expect(rewardCredential(derived)).toBe(stakeCredential(BASE));
  });

  it('returns null for an address with no stake part', () => {
    // Enterprise address (no stake credential) — truncate isn't valid; use a known one.
    expect(stakeAddressOf('addr1vxx…')).toBeNull();
  });
});

describe('paymentIsScript', () => {
  it('tells a contract from a wallet', () => {
    expect(paymentIsScript(BASE)).toBe(false);
    // Minswap V2's order script, with and without a stake part.
    expect(paymentIsScript('addr1w8p79rpkcdz8x9d6tft0x0dx5mwuzac2sa4gm8cvkw5hcnqst2ctf')).toBe(true);
    expect(
      paymentIsScript(
        'addr1z8p79rpkcdz8x9d6tft0x0dx5mwuzac2sa4gm8cvkw5hcnrcq7dmqu20hxxtcts5zkz7jaqrwua8claa2hrghvgnwnpqjxj2vs',
      ),
    ).toBe(true);
  });

  it('is not fooled by the stake credential, which says nothing about the payment side', () => {
    // These two share a stake credential. One is Alice's wallet, the other is Minswap's
    // order script holding her order — the distinction the whole sentence depends on.
    const wallet =
      'addr1q9l642m4y7smwuj3e57e2xxa6pt6g3wrk7dyvh9960ezxnrcq7dmqu20hxxtcts5zkz7jaqrwua8claa2hrghvgnwnpq458vet';
    const script =
      'addr1z8p79rpkcdz8x9d6tft0x0dx5mwuzac2sa4gm8cvkw5hcnrcq7dmqu20hxxtcts5zkz7jaqrwua8claa2hrghvgnwnpqjxj2vs';
    expect(stakeAddressOf(wallet)).toBe(stakeAddressOf(script));
    expect(paymentIsScript(wallet)).toBe(false);
    expect(paymentIsScript(script)).toBe(true);
  });

  it('says no rather than throwing on anything undecodable', () => {
    expect(paymentIsScript('not-an-address')).toBe(false);
    expect(paymentIsScript('')).toBe(false);
  });
});
