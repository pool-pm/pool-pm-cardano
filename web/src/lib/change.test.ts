import { describe, it, expect } from 'vitest';
import type { AssetInfo, TxInput, TxOutputInfo } from './types';
import { nonChangeOutputs, parseQuantity, addQuantities } from './change';

// --- Test addresses ---
// Real addresses from the NIGHT swap tx (d938b9dc...).
// All share stake credential 78079bb0714fb98cbc2e141585e97403773a7c7fbd55c68bb11374c2.

/** Wallet input address (type 0 = key pay + key stake), pay cred 7faaab75... */
const WALLET_ADDR_A =
  'addr1q9l642m4y7smwuj3e57e2xxa6pt6g3wrk7dyvh9960ezxnrcq7dmqu20hxxtcts5zkz7jaqrwua8claa2hrghvgnwnpq458vet';
/** Script input address (type 1 = script pay + key stake), pay cred 5986bfcc... */
const SCRIPT_ADDR =
  'addr1z9vcd07vpjluvr0v3hu8w9wvjhgrs9a2cwrtp6wn8ksrkwtcq7dmqu20hxxtcts5zkz7jaqrwua8claa2hrghvgnwnpqnw4yz0';
/** Output address (type 0), different pay cred e16d02d4..., same stake cred */
const WALLET_ADDR_B =
  'addr1q8sk6qk5pmlpf087frthhxwft8zxwac7h4ynef89l0xxvhncq7dmqu20hxxtcts5zkz7jaqrwua8claa2hrghvgnwnpqy0p80q';
/** Another wallet address (type 0), different pay cred 7bfaa286..., same stake cred */
const WALLET_ADDR_C =
  'addr1q9al4g5x923facu4rmn64jpdj9hzlemv49egt6wucj6na6tcq7dmqu20hxxtcts5zkz7jaqrwua8claa2hrghvgnwnpq4tlyfd';
/** External address (completely different stake cred) */
const EXTERNAL_ADDR =
  'addr1qx2fxv2umyhttkxyxp8x0dlpdt3k6cwng5pxj3jhsydzer3jcu5d8ps7zex2k2xt3uqxgjqnnj83ws8lhrn648jjxtwq2ytjqp';

// --- Helpers ---

function asset(fingerprint: string, quantity: string): AssetInfo {
  return { fingerprint, quantity };
}

function input(address: string, lovelace: string, assets: AssetInfo[] = []): TxInput {
  return { tx_hash: '00'.repeat(32), index: 0, address, lovelace, assets };
}

function output(address: string, lovelace: string, assets: AssetInfo[] = []): TxOutputInfo {
  return { address, lovelace, assets };
}

// --- parseQuantity / addQuantities ---

describe('parseQuantity', () => {
  it('parses integer', () => {
    expect(parseQuantity('100')).toEqual([100n, 0]);
  });
  it('parses decimal', () => {
    expect(parseQuantity('1.5')).toEqual([15n, 1]);
  });
  it('parses many decimals', () => {
    expect(parseQuantity('291922.894186')).toEqual([291922894186n, 6]);
  });
  it('parses zero', () => {
    expect(parseQuantity('0')).toEqual([0n, 0]);
  });
});

describe('addQuantities', () => {
  it('adds same scale', () => {
    expect(addQuantities([15n, 1], [25n, 1])).toEqual([40n, 1]);
  });
  it('aligns different scales', () => {
    // 1.5 + 0.25 = 1.75
    expect(addQuantities([15n, 1], [25n, 2])).toEqual([175n, 2]);
  });
});

// --- nonChangeOutputs ---

describe('nonChangeOutputs', () => {
  describe('simple ADA transfers', () => {
    it('hides pure ADA change to same address', () => {
      const ins = [input(WALLET_ADDR_A, '10000000')];
      const outs = [
        output(EXTERNAL_ADDR, '8000000'),
        output(WALLET_ADDR_A, '1800000'), // change
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result).toEqual([outs[0]]);
    });

    it('hides pure ADA change to different pay cred (same stake cred)', () => {
      const ins = [input(WALLET_ADDR_A, '10000000')];
      const outs = [
        output(EXTERNAL_ADDR, '8000000'),
        output(WALLET_ADDR_B, '1800000'), // change to different derivation
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result).toEqual([outs[0]]);
    });

    it('shows output to external address', () => {
      const ins = [input(WALLET_ADDR_A, '5000000')];
      const outs = [output(EXTERNAL_ADDR, '4800000')];
      const result = nonChangeOutputs(ins, outs);
      expect(result).toEqual([outs[0]]);
    });

    it('shows all outputs when no inputs match', () => {
      const ins = [input(EXTERNAL_ADDR, '5000000')];
      const outs = [output(WALLET_ADDR_A, '2000000'), output(WALLET_ADDR_B, '2800000')];
      const result = nonChangeOutputs(ins, outs);
      expect(result.length).toBe(2);
    });
  });

  describe('ADA received (more ADA out than in for credential group)', () => {
    it('shows outputs when wallet receives extra ADA (e.g. from withdrawal)', () => {
      // Wallet puts in 10 ADA, gets back 15 ADA (5 ADA from withdrawal/external)
      const ins = [input(WALLET_ADDR_A, '10000000'), input(EXTERNAL_ADDR, '6000000')];
      const outs = [
        output(WALLET_ADDR_B, '15000000'), // wallet gets more than it put in
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result).toEqual([outs[0]]);
    });

    it('hides change when wallet ADA out <= ADA in', () => {
      // Wallet sends 2 ADA to external, change goes back to wallet
      const ins = [input(WALLET_ADDR_A, '10000000')];
      const outs = [
        output(EXTERNAL_ADDR, '2000000'), // sent
        output(WALLET_ADDR_B, '7800000'), // change: 7.8M <= 10M input
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result).toEqual([outs[0]]);
    });
  });

  describe('asset received (new asset not in inputs)', () => {
    it('shows output with asset not present in any input', () => {
      const NIGHT = 'asset1wd3llgkhsw6etxf2yca6cgk9ssrpva3wf0pq9a';
      const ins = [input(WALLET_ADDR_A, '10000000')]; // no NIGHT in inputs
      const outs = [
        output(WALLET_ADDR_B, '1200000', [asset(NIGHT, '100')]), // received NIGHT
        output(WALLET_ADDR_C, '8600000'), // pure ADA change
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result).toEqual([outs[0]]);
    });
  });

  describe('asset quantity comparison', () => {
    const NIGHT = 'asset1wd3llgkhsw6etxf2yca6cgk9ssrpva3wf0pq9a';

    it('shows outputs when wallet receives more of an existing asset (DEX swap)', () => {
      // Wallet has 100 NIGHT, DEX sends 200 more, wallet output has 300 NIGHT
      const ins = [
        input(WALLET_ADDR_A, '10000000', [asset(NIGHT, '100')]),
        input(SCRIPT_ADDR, '2000000', [asset(NIGHT, '200')]), // DEX input (script, separate group)
      ];
      const outs = [
        output(WALLET_ADDR_B, '1200000', [asset(NIGHT, '300')]), // wallet got 300, had 100 → received
        output(SCRIPT_ADDR, '9000000'), // DEX change
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result.map((o) => o.address)).toContain(WALLET_ADDR_B);
    });

    it('hides change when asset quantity does not exceed input', () => {
      // Wallet has 300 NIGHT, sends 200 to external, change has 100
      const ins = [input(WALLET_ADDR_A, '10000000', [asset(NIGHT, '300')])];
      const outs = [
        output(EXTERNAL_ADDR, '2000000', [asset(NIGHT, '200')]), // sent
        output(WALLET_ADDR_B, '7800000', [asset(NIGHT, '100')]), // change
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result).toEqual([outs[0]]);
    });

    it('hides change when asset quantity equals input exactly', () => {
      const ins = [input(WALLET_ADDR_A, '10000000', [asset(NIGHT, '100')])];
      const outs = [
        output(EXTERNAL_ADDR, '8000000'),
        output(WALLET_ADDR_A, '1800000', [asset(NIGHT, '100')]), // same qty back
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result).toEqual([outs[0]]);
    });

    it('shows output when asset quantity exceeds input with decimals', () => {
      const ins = [input(WALLET_ADDR_A, '5000000', [asset(NIGHT, '100.5')])];
      const outs = [
        output(WALLET_ADDR_B, '4800000', [asset(NIGHT, '100.6')]), // 0.1 more
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result).toEqual([outs[0]]);
    });

    it('hides change when decimal quantity does not exceed input', () => {
      const ins = [input(WALLET_ADDR_A, '5000000', [asset(NIGHT, '100.5')])];
      const outs = [
        output(EXTERNAL_ADDR, '2000000', [asset(NIGHT, '1')]),
        output(WALLET_ADDR_B, '2800000', [asset(NIGHT, '99.5')]), // less
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result).toEqual([outs[0]]);
    });
  });

  describe('script-payment vs key-payment separation', () => {
    const NIGHT = 'asset1wd3llgkhsw6etxf2yca6cgk9ssrpva3wf0pq9a';

    it('does not let script input assets pollute wallet change detection', () => {
      // Real NIGHT tx scenario: wallet input (no NIGHT) + DEX script input (has NIGHT)
      // Wallet receives NIGHT in output — should NOT be hidden as change
      const ins = [
        input(WALLET_ADDR_A, '3320166948152'), // wallet, no NIGHT
        input(SCRIPT_ADDR, '1698140', [asset(NIGHT, '583845.788374')]), // DEX script
      ];
      const outs = [
        output(WALLET_ADDR_B, '1176630', [asset(NIGHT, '291922.894186')]), // receive from swap
        output(SCRIPT_ADDR, '1698140', [asset(NIGHT, '583845.788374')]), // script change
        output(WALLET_ADDR_C, '3320165444567'), // ADA change
      ];
      const result = nonChangeOutputs(ins, outs);
      // Wallet output with NIGHT must be shown
      expect(result.map((o) => o.address)).toContain(WALLET_ADDR_B);
      // Pure ADA change to wallet must be hidden
      expect(result.map((o) => o.address)).not.toContain(WALLET_ADDR_C);
    });

    it('script output matching script input is hidden as change', () => {
      const ins = [input(WALLET_ADDR_A, '10000000'), input(SCRIPT_ADDR, '2000000', [asset(NIGHT, '500')])];
      const outs = [
        output(SCRIPT_ADDR, '2000000', [asset(NIGHT, '500')]), // exact same back to script
        output(WALLET_ADDR_B, '9800000'), // wallet change
      ];
      const result = nonChangeOutputs(ins, outs);
      // Both are change
      expect(result.length).toBe(0);
    });

    it('DEX pool output shown but assets stripped when it received extra ADA', () => {
      // Minswap scenario: script pool receives extra ADA from swap
      const ins = [input(SCRIPT_ADDR, '575000000000', [asset(NIGHT, '27000000')]), input(WALLET_ADDR_A, '700000000')];
      const outs = [
        output(SCRIPT_ADDR, '575100000000', [asset(NIGHT, '26995000')]), // pool got more ADA
        output(WALLET_ADDR_B, '2000000', [asset(NIGHT, '5000')]), // user received NIGHT
        output(WALLET_ADDR_C, '597000000'), // wallet ADA change
      ];
      const result = nonChangeOutputs(ins, outs);
      // Script output shown (received extra ADA) but NIGHT stripped (exists in input)
      const scriptOut = result.find((o) => o.address === SCRIPT_ADDR);
      expect(scriptOut).toBeDefined();
      expect(scriptOut!.assets.length).toBe(0);
      // User's NIGHT output is shown (new asset not in wallet group)
      expect(result.map((o) => o.address)).toContain(WALLET_ADDR_B);
    });
  });

  describe('multiple assets', () => {
    const NIGHT = 'asset1wd3llgkhsw6etxf2yca6cgk9ssrpva3wf0pq9a';
    const HOSKY = 'asset1hosky0000000000000000000000000000000000';

    it('shows output when one of multiple assets exceeds input, strips the other', () => {
      const ins = [input(WALLET_ADDR_A, '10000000', [asset(NIGHT, '100'), asset(HOSKY, '50')])];
      const outs = [
        output(WALLET_ADDR_B, '9800000', [asset(NIGHT, '101'), asset(HOSKY, '50')]), // NIGHT exceeds
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result.length).toBe(1);
      // NIGHT (101 > 100) kept, HOSKY (50 <= 50) stripped
      expect(result[0].assets).toEqual([asset(NIGHT, '101')]);
    });

    it('hides change when all assets at or below input quantities', () => {
      const ins = [input(WALLET_ADDR_A, '10000000', [asset(NIGHT, '100'), asset(HOSKY, '50')])];
      const outs = [
        output(EXTERNAL_ADDR, '2000000', [asset(NIGHT, '10'), asset(HOSKY, '10')]),
        output(WALLET_ADDR_B, '7800000', [asset(NIGHT, '90'), asset(HOSKY, '40')]), // change
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result).toEqual([outs[0]]);
    });

    it('shows output with a mix of known and unknown assets, stripping known', () => {
      const ins = [input(WALLET_ADDR_A, '10000000', [asset(NIGHT, '100')])];
      const outs = [
        output(WALLET_ADDR_B, '2000000', [asset(NIGHT, '50'), asset(HOSKY, '10')]), // HOSKY is new
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result.length).toBe(1);
      // NIGHT (known) is stripped, only HOSKY (new) remains
      expect(result[0].assets).toEqual([asset(HOSKY, '10')]);
    });
  });

  describe('multiple inputs aggregation', () => {
    const NIGHT = 'asset1wd3llgkhsw6etxf2yca6cgk9ssrpva3wf0pq9a';

    it('sums asset quantities across multiple inputs from same wallet', () => {
      // Two wallet UTXOs with 50 NIGHT each = 100 total
      const ins = [
        input(WALLET_ADDR_A, '5000000', [asset(NIGHT, '50')]),
        input(WALLET_ADDR_B, '5000000', [asset(NIGHT, '50')]),
      ];
      const outs = [
        output(EXTERNAL_ADDR, '2000000', [asset(NIGHT, '30')]),
        output(WALLET_ADDR_C, '7800000', [asset(NIGHT, '70')]), // 70 <= 100, it's change
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result).toEqual([outs[0]]);
    });

    it('sums lovelace across multiple inputs from same wallet', () => {
      const ins = [input(WALLET_ADDR_A, '5000000'), input(WALLET_ADDR_B, '5000000')];
      const outs = [
        output(EXTERNAL_ADDR, '2000000'),
        output(WALLET_ADDR_C, '7800000'), // 7.8M <= 10M
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result).toEqual([outs[0]]);
    });
  });

  describe('multiple outputs in same credential group', () => {
    const NIGHT = 'asset1wd3llgkhsw6etxf2yca6cgk9ssrpva3wf0pq9a';

    it('sums output quantities across group before comparing to inputs', () => {
      // Input: 100 NIGHT. Two outputs to same wallet: 60 + 50 = 110 > 100 → received
      const ins = [
        input(WALLET_ADDR_A, '10000000', [asset(NIGHT, '100')]),
        input(EXTERNAL_ADDR, '5000000', [asset(NIGHT, '10')]),
      ];
      const outs = [
        output(WALLET_ADDR_B, '5000000', [asset(NIGHT, '60')]),
        output(WALLET_ADDR_C, '4800000', [asset(NIGHT, '50')]),
      ];
      const result = nonChangeOutputs(ins, outs);
      // Both outputs in the group are shown since group received extra
      expect(result.length).toBe(2);
    });

    it('hides all outputs in group when total does not exceed', () => {
      const ins = [input(WALLET_ADDR_A, '10000000', [asset(NIGHT, '100')])];
      const outs = [
        output(EXTERNAL_ADDR, '2000000'),
        output(WALLET_ADDR_B, '4000000', [asset(NIGHT, '40')]),
        output(WALLET_ADDR_C, '3800000', [asset(NIGHT, '60')]),
        // total: 40 + 60 = 100, not > 100
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result).toEqual([outs[0]]);
    });
  });

  describe('edge cases', () => {
    it('handles tx with no inputs (empty)', () => {
      const outs = [output(WALLET_ADDR_A, '5000000')];
      const result = nonChangeOutputs([], outs);
      expect(result).toEqual(outs);
    });

    it('handles tx with no outputs', () => {
      const ins = [input(WALLET_ADDR_A, '5000000')];
      const result = nonChangeOutputs(ins, []);
      expect(result).toEqual([]);
    });

    it('handles inputs with no address (unresolved)', () => {
      const ins = [
        input(WALLET_ADDR_A, '5000000'),
        { tx_hash: '00'.repeat(32), index: 1, address: null, lovelace: '3000000' },
      ];
      const outs = [
        output(EXTERNAL_ADDR, '2000000'),
        output(WALLET_ADDR_B, '4800000'), // change
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result).toEqual([outs[0]]);
    });

    it('handles inputs with no assets field', () => {
      const ins: TxInput[] = [{ tx_hash: '00'.repeat(32), index: 0, address: WALLET_ADDR_A, lovelace: '5000000' }];
      const outs = [output(EXTERNAL_ADDR, '3000000'), output(WALLET_ADDR_B, '1800000')];
      const result = nonChangeOutputs(ins, outs);
      expect(result).toEqual([outs[0]]);
    });

    it('handles zero-quantity assets', () => {
      const NIGHT = 'asset1wd3llgkhsw6etxf2yca6cgk9ssrpva3wf0pq9a';
      const ins = [input(WALLET_ADDR_A, '5000000', [asset(NIGHT, '0')])];
      const outs = [output(WALLET_ADDR_B, '4800000', [asset(NIGHT, '0')])];
      const result = nonChangeOutputs(ins, outs);
      expect(result.length).toBe(0);
    });
  });

  describe('change asset stripping', () => {
    const NIGHT = 'asset1wd3llgkhsw6etxf2yca6cgk9ssrpva3wf0pq9a';
    const DJED = 'asset15f3ymkjafxxeunv5gtdl54g5qs8ty9k84tq94x';
    const HOSKY = 'asset1hosky0000000000000000000000000000000000';

    it('strips change assets from output shown due to new asset', () => {
      const ins = [input(WALLET_ADDR_A, '10000000', [asset(NIGHT, '1000')])];
      const outs = [output(WALLET_ADDR_B, '2000000', [asset(NIGHT, '0.001'), asset(DJED, '500')])];
      const result = nonChangeOutputs(ins, outs);
      expect(result.length).toBe(1);
      expect(result[0].assets).toEqual([asset(DJED, '500')]);
    });

    it('strips multiple change assets, keeps only new ones', () => {
      const ins = [input(WALLET_ADDR_A, '10000000', [asset(NIGHT, '1000'), asset(HOSKY, '500')])];
      const outs = [
        output(WALLET_ADDR_B, '2000000', [asset(HOSKY, '0.01'), asset(DJED, '100'), asset(NIGHT, '0.001')]),
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result.length).toBe(1);
      expect(result[0].assets).toEqual([asset(DJED, '100')]);
    });

    it('does not strip assets from output to external address (no group)', () => {
      const ins = [input(WALLET_ADDR_A, '10000000')];
      const outs = [output(EXTERNAL_ADDR, '2000000', [asset(NIGHT, '10'), asset(DJED, '5')])];
      const result = nonChangeOutputs(ins, outs);
      expect(result[0].assets.length).toBe(2);
    });

    it('strips change assets but keeps exceeded assets in group outputs', () => {
      // Wallet has 100 NIGHT + 50 HOSKY. Receives 200 more NIGHT from external.
      // Output has 300 NIGHT (exceeds 100 input) + 50 HOSKY (change).
      const ins = [
        input(WALLET_ADDR_A, '10000000', [asset(NIGHT, '100'), asset(HOSKY, '50')]),
        input(EXTERNAL_ADDR, '5000000', [asset(NIGHT, '200')]),
      ];
      const outs = [output(WALLET_ADDR_B, '14000000', [asset(HOSKY, '50'), asset(NIGHT, '300')])];
      const result = nonChangeOutputs(ins, outs);
      expect(result.length).toBe(1);
      // HOSKY (50 <= 50) stripped, NIGHT (300 > 100) kept
      expect(result[0].assets).toEqual([asset(NIGHT, '300')]);
    });

    it('real DEX swap: strips dust, keeps only received DjedMicroUSD', () => {
      const TOKEN_A = 'asset17ssyw22hngef29v0w3syf73t0snvvzx70tn84f';
      const RSUNDAE = 'asset1lygkpd9d4qufuhveqfend5s8ul6zgfurhy9t24';
      const ins = [
        input(WALLET_ADDR_A, '10000000', [asset(TOKEN_A, '1000'), asset(RSUNDAE, '1')]),
        input(SCRIPT_ADDR, '3000000', [asset(DJED, '400000')]),
      ];
      const outs = [
        output(WALLET_ADDR_B, '2000000', [asset(RSUNDAE, '1'), asset(TOKEN_A, '0.001'), asset(DJED, '44683')]),
        output(WALLET_ADDR_C, '7800000'),
      ];
      const result = nonChangeOutputs(ins, outs);
      expect(result.length).toBe(1);
      expect(result[0].assets).toEqual([asset(DJED, '44683')]);
    });
  });
});
