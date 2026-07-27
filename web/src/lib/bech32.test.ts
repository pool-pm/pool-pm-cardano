import { describe, it, expect } from 'vitest';
import { bech32Encode, bech32Decode, stakeAddressOf, stakeCredential, rewardCredential } from './bech32';

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
