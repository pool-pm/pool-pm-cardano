import { describe, it, expect } from 'vitest';
import type { AssetInfo, BlockTx, DelegationInfo, MetadataEntry, TxInput, TxOutputInfo } from './types';
import { describeTx, partyForAddress, shortAddress } from './intent';

// --- Test addresses ---
// Real mainnet addresses. ALICE_A and ALICE_B share a stake credential (one wallet,
// two payment addresses); BOB and CAROL are separate wallets.

const ALICE_A =
  'addr1q9l642m4y7smwuj3e57e2xxa6pt6g3wrk7dyvh9960ezxnrcq7dmqu20hxxtcts5zkz7jaqrwua8claa2hrghvgnwnpq458vet';
const ALICE_B =
  'addr1q8sk6qk5pmlpf087frthhxwft8zxwac7h4ynef89l0xxvhncq7dmqu20hxxtcts5zkz7jaqrwua8claa2hrghvgnwnpqy0p80q';
/** A third address of Alice's, used only as an output — her change destination. */
const ALICE_C =
  'addr1q9al4g5x923facu4rmn64jpdj9hzlemv49egt6wucj6na6tcq7dmqu20hxxtcts5zkz7jaqrwua8claa2hrghvgnwnpq4tlyfd';
/** Alice's reward account — the stake address behind ALICE_A / ALICE_B. */
const ALICE_STAKE = 'stake1u9uq0xasw98mnr9u9c2ptp0fwsphwwnu0774t35tkyfhfsseqmky2';
const BOB = 'addr1qx2fxv2umyhttkxyxp8x0dlpdt3k6cwng5pxj3jhsydzer3jcu5d8ps7zex2k2xt3uqxgjqnnj83ws8lhrn648jjxtwq2ytjqp';
const CAROL = 'addr1q8e533g8x64jzcr0cwn8meau8wuzk0m5ca60sfazglxhat73zzqrg5denlr3pcre4utltelfte0ts5zpnhjjpwu6aldq07s2rm';

/** Minswap's "Batch Order" script — where a swap order is posted. */
const MINSWAP_ORDER = 'addr1wyx22z2s4kasd3w976pnjf9xdty88epjqfvgkmfnscpd0rg3z8y6v';
/**
 * Minswap V2's order script paired with ALICE's stake credential — the real shape of a
 * V2 order address, which `stakeAddressOf` resolves to Alice's own reward account.
 */
const MINSWAP_V2_ORDER_ALICE =
  'addr1z8p79rpkcdz8x9d6tft0x0dx5mwuzac2sa4gm8cvkw5hcnrcq7dmqu20hxxtcts5zkz7jaqrwua8claa2hrghvgnwnpqjxj2vs';
/** The reward account of Minswap's order script — a real one, seen on live traffic. */
const MINSWAP_ORDER_REWARDS = 'stake17y02a946720zw6pw50upt2arvxsvvpvaghjtl054h0f0gjsfyjz59';
/** Minswap's "Liquidity Pool" script — a role with no verb of its own. */
const MINSWAP_POOL =
  'addr1z9tu3ecccgqlhgg2nkshfrt8td2zs8fmrwvrchgksl78x96j2c79gy9l76sdg0xwhd7r0c0kna0tycz4y5s6mlenh8pq26n58l';
/** jpg.store's marketplace script. */
const JPGSTORE =
  'addr1zxj47sy4qxlktqzmkrw8dahe46gtv8seakrshsqz26qnvzypw288a4x0xf8pxgcntelxmyclq83s0ykeehchz2wtspksr3q9nx';

const POOL_ID = 'pool1qqqqpanw9zc0rzh0yha3rxcs3lstnqhkqsdz6vwd4jjagpq0dcq';
const DREP_ID = 'drep1xyz00000000000000000000000000000000000000000000000';

// --- Helpers ---

function asset(fingerprint: string, quantity = '1'): AssetInfo {
  return { fingerprint, quantity, size: 256 };
}

function input(address: string, lovelace: string, extra: Partial<TxInput> = {}): TxInput {
  return { tx_hash: '00'.repeat(32), index: 0, address, lovelace, assets: [], ...extra };
}

/** The pseudo-input the server appends for a reward-account withdrawal. */
function withdrawal(stakeAddress: string, lovelace: string): TxInput {
  return { tx_hash: '', index: -1, address: stakeAddress, lovelace, assets: [] };
}

function output(address: string, lovelace: string, assets: AssetInfo[] = [], handle?: string): TxOutputInfo {
  return { address, lovelace, assets, handle };
}

/** A CIP-20 message, as the wire now carries it: label 674 holding `{msg: [lines]}`. */
function message(lines: string[]): MetadataEntry[] {
  return [{ label: 674, value: { map: [{ k: 'msg', v: lines }] } }];
}

function tx(parts: Partial<BlockTx>): BlockTx {
  return { hash: 'ab'.repeat(32), fee: '170000', size: 300, inputs: [], outputs: [], ...parts };
}

function delegation(parts: Partial<DelegationInfo>): DelegationInfo {
  return { stake_address: ALICE_STAKE, live_stake: '50000000000', ...parts };
}

// --- shortAddress ---

describe('shortAddress', () => {
  it('keeps both ends identifying', () => {
    expect(shortAddress(BOB)).toBe('addr1qx2…tjqp');
  });

  it('leaves an already-short label alone', () => {
    expect(shortAddress('addr1qx2y')).toBe('addr1qx2y');
  });
});

// --- partyForAddress ---

describe('partyForAddress', () => {
  it('prefers an ADA Handle', () => {
    const party = partyForAddress(BOB, 'bob');
    expect(party).toMatchObject({ label: '$bob', kind: 'handle', href: '/' + BOB });
  });

  it('names a known script by its dApp', () => {
    expect(partyForAddress(MINSWAP_ORDER)).toMatchObject({ label: 'MINSWAP', kind: 'app' });
  });

  it('falls back to a truncated address', () => {
    expect(partyForAddress(BOB)).toMatchObject({ label: 'addr1qx2…tjqp', kind: 'address' });
  });

  it('leaves a Byron address unlinked', () => {
    expect(partyForAddress('DdzFFzCqrhsf6hiTY').href).toBeUndefined();
  });
});

// --- Transfers ---

describe('describeTx: transfers', () => {
  it('reads a plain payment as a sentence', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000', { handle: 'alice' })],
        outputs: [output(BOB, '1234000000'), output(ALICE_A, '8000000')],
      }),
    )!;
    expect(intent.subject).toMatchObject({ label: '$alice' });
    expect(intent.verb).toBe('SENT');
    expect(intent.amount).toEqual({ quantity: '1234000000' });
    expect(intent.preposition).toBe('TO');
    expect(intent.targets).toHaveLength(1);
    expect(intent.targets[0].party).toMatchObject({ label: 'addr1qx2…tjqp' });
  });

  it('names the recipient by handle when it has one', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000')],
        outputs: [output(BOB, '5000000', [], 'bob')],
      }),
    )!;
    expect(intent.targets[0].party).toMatchObject({ label: '$bob', kind: 'handle' });
  });

  it('names the account when the sender spent from several of its addresses', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '4000000'), input(ALICE_B, '9000000')],
        outputs: [output(BOB, '12000000')],
      }),
    )!;
    // Two addresses, one account — the account is what they share.
    expect(intent.subject).toMatchObject({ id: ALICE_STAKE, label: 'stake1u9…mky2' });
    expect(intent.verb).toBe('SENT');
  });

  it('finds the handle on any address of the sending account', () => {
    const intent = describeTx(
      tx({
        // The handle sits on the address that contributed least.
        inputs: [input(ALICE_A, '4000000', { handle: 'alice' }), input(ALICE_B, '9000000')],
        outputs: [output(BOB, '12000000')],
      }),
    )!;
    expect(intent.subject).toMatchObject({ label: '$alice' });
  });

  it('merges recipients that share a stake credential and sums them', () => {
    const intent = describeTx(
      tx({
        inputs: [input(BOB, '30000000')],
        // Three addresses of Alice's one account.
        outputs: [output(ALICE_A, '5000000'), output(ALICE_B, '9000000'), output(ALICE_C, '2000000')],
      }),
    )!;
    expect(intent.targets).toHaveLength(1);
    expect(intent.amount).toEqual({ quantity: '16000000' });
    expect(intent.targets[0].party).toMatchObject({ id: ALICE_STAKE });
  });

  it('names a merged recipient by the account handle when it has one', () => {
    const intent = describeTx(
      tx({
        inputs: [input(BOB, '30000000')],
        outputs: [output(ALICE_A, '5000000'), output(ALICE_B, '9000000', [], 'alice')],
      }),
    )!;
    expect(intent.targets[0].party).toMatchObject({ label: '$alice' });
  });

  it('keeps naming a lone recipient by its own address', () => {
    const intent = describeTx(tx({ inputs: [input(BOB, '30000000')], outputs: [output(ALICE_A, '5000000')] }))!;
    expect(intent.targets[0].party).toMatchObject({ id: ALICE_A });
  });

  it('does not read a reward withdrawal as a settled batch', () => {
    // A withdrawal is a pseudo-input carrying a reward address, not a UTXO anyone can
    // post an order in — and this reward account is Minswap's order script's own, so it
    // resolves to a dApp with the `order` role like any of its addresses would.
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '4000000'), withdrawal(MINSWAP_ORDER_REWARDS, '7000000')],
        outputs: [output(ALICE_A, '10800000')],
      }),
    )!;
    expect(intent.verb).toBe('WITHDREW');
  });

  it('names the funders when more than one wallet paid', () => {
    // "Who sent" has no single answer here, so it gets several — named, not counted.
    // Addresses sharing a stake credential have already been folded into one account by
    // now, so what's left really is distinct parties, biggest contributor first.
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '4000000'), input(BOB, '9000000')],
        outputs: [output(CAROL, '12000000')],
      }),
    )!;
    expect(intent.subject).toBeUndefined();
    expect(intent.subjects?.map((s) => s.label)).toEqual([partyForAddress(BOB).label, partyForAddress(ALICE_A).label]);
    expect(intent.verb).toBe('SENT');
    expect(intent.amount).toEqual({ quantity: '12000000' });
    expect(intent.targets[0].party).toMatchObject({ id: CAROL });
  });

  it('names a funding account by its handle, and by its stake address when it has none', () => {
    const intent = describeTx(
      tx({
        // Alice funds from two of her own addresses: one account, named once.
        inputs: [input(ALICE_A, '4000000'), input(ALICE_B, '1000000'), input(BOB, '9000000', { handle: 'bob' })],
        outputs: [output(CAROL, '13000000')],
      }),
    )!;
    expect(intent.subjects?.map((s) => s.label)).toEqual(['$bob', 'stake1u9…mky2']);
  });

  it('does not count a reclaimed order UTXO as a second funder', () => {
    // Cancelling a DEX order spends the order script alongside the wallet's own funds.
    // The order address carries the canceller's stake credential, which is what tells it
    // apart from a batcher spending somebody else's order.
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '4000000'), input(MINSWAP_V2_ORDER_ALICE, '9000000')],
        outputs: [output(ALICE_A, '12500000')],
      }),
    )!;
    expect(intent.subject).toMatchObject({ id: ALICE_A });
  });

  it('sums several recipients into one total and lists who got it', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '20000000')],
        outputs: [output(BOB, '5000000'), output(CAROL, '9000000')],
      }),
    )!;
    // An amount stacked above each name reads as "sent TO 9,000000" — the sum doesn't.
    expect(intent.amount).toEqual({ quantity: '14000000' });
    expect(intent.targets.map((t) => t.amount)).toEqual([undefined, undefined]);
    expect(intent.targets.map((t) => t.party?.id)).toEqual([CAROL, BOB]);
    expect(intent.hiddenTargets).toBe(0);
  });

  it('merges several outputs to the same recipient', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '20000000')],
        outputs: [output(BOB, '5000000'), output(BOB, '9000000')],
      }),
    )!;
    expect(intent.targets).toHaveLength(1);
    expect(intent.amount).toEqual({ quantity: '14000000' });
  });

  it('ignores value landing on another address of the sender', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '20000000')],
        // ALICE_C receives more than it put in, so it reads as a receipt to
        // `nonChangeOutputs` — but it is still Alice's own account.
        outputs: [output(BOB, '5000000'), output(ALICE_C, '14800000')],
      }),
    )!;
    expect(intent.targets).toHaveLength(1);
    expect(intent.targets[0].party).toMatchObject({ id: BOB });
    expect(intent.amount).toEqual({ quantity: '5000000' });
  });

  it('leads with the metadata when a tx moved nothing but wrote something', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000')],
        outputs: [output(ALICE_A, '9800000')],
        // Label 1's own keys, which is what these ~9,800 txs a month are actually for.
        metadata: message(['timestamp absolute_slot']),
      }),
    )!;
    // "MOVED 9.8 ₳" describes the mechanism and hides the purpose: nothing was paid to
    // anyone, and the tx exists to write this.
    expect(intent.verb).toBe('WROTE');
    expect(intent.note).toEqual(['timestamp absolute_slot']);
    expect(intent.amount).toBeUndefined();
    expect(intent.messageRead).toBe(true);
  });

  it('reads a self-transfer as a move', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000'), input(ALICE_B, '5000000')],
        outputs: [output(ALICE_A, '14800000')],
      }),
    )!;
    expect(intent.verb).toBe('MOVED');
    expect(intent.amount).toEqual({ quantity: '14800000' });
    expect(intent.targets).toEqual([]);
  });
});

// --- Tokens ---

describe('describeTx: tokens', () => {
  it('headlines the token, not the min-UTXO ADA carrying it', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000')],
        outputs: [output(BOB, '1500000', [asset('asset1nft')])],
      }),
    )!;
    expect(intent.amount).toBeUndefined();
    expect(intent.assets).toEqual([asset('asset1nft')]);
  });

  it('keeps the ADA when it is more than dust', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '600000000')],
        outputs: [output(BOB, '500000000', [asset('asset1tok', '25')])],
      }),
    )!;
    expect(intent.amount).toEqual({ quantity: '500000000' });
    expect(intent.assets).toHaveLength(1);
  });
});

// --- dApps ---

describe('describeTx: dApps', () => {
  it('reads an order posted to a DEX as a swap in progress', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '110000000', { handle: 'alice' })],
        outputs: [output(MINSWAP_ORDER, '100000000'), output(ALICE_A, '9000000')],
      }),
    )!;
    // Present tense: this order hasn't executed. No datum here, so the assets go unnamed
    // — but the tense is still the truth about it.
    expect(intent.verb).toBe('SWAPPING');
    expect(intent.amount).toEqual({ quantity: '100000000' });
    expect(intent.via).toMatchObject({ label: 'MINSWAP', kind: 'app' });
    expect(intent.targets).toEqual([]);
  });

  it('reads through the batcher fee that comes with an order', () => {
    // The shape almost every real DEX order has: the order itself, plus a small fee
    // output to a separate address.
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '110000000')],
        outputs: [output(MINSWAP_ORDER, '73000000'), output(BOB, '2000000')],
      }),
    )!;
    expect(intent.verb).toBe('SWAPPING');
    expect(intent.amount).toEqual({ quantity: '73000000' });
    expect(intent.via).toMatchObject({ label: 'MINSWAP' });
    expect(intent.targets).toEqual([]);
  });

  it('does not swallow a payment larger than the dApp output', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '110000000')],
        outputs: [output(MINSWAP_ORDER, '2000000'), output(BOB, '73000000')],
      }),
    )!;
    expect(intent.verb).toBe('SENT');
    expect(intent.targets).toHaveLength(2);
  });

  it('reads an order posted to a script that carries the sender’s own stake key', () => {
    // Minswap V2's order address is its script hash plus *the user's* stake credential,
    // so the user keeps staking while the order waits to be filled. Folding outputs by
    // stake credential therefore read the order as Alice's own change and dropped it,
    // and a DexHunter trade ended up headlined by the 2 ₳ fee that was left over.
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '600000000')],
        outputs: [output(MINSWAP_V2_ORDER_ALICE, '596000000'), output(ALICE_A, '3000000')],
      }),
    )!;
    expect(intent.verb).toBe('SWAPPING');
    expect(intent.amount).toEqual({ quantity: '596000000' });
    expect(intent.via).toMatchObject({ label: 'MINSWAP' });
  });

  it('reads a pending swap out of the order datum', () => {
    // A real Minswap V2 order: ADA in, WorldMobileTokenX out.
    const datum =
      'd8799fd8799f581c636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5affd8799fd8799f581c636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5affd8799fd8799fd8799f581ce39b5f40aa85fbc121a625d777a776eca1cb4c923426949c997d8828ffffffffd87980d8799fd8799f581c636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5affd8799fd8799fd8799f581ce39b5f40aa85fbc121a625d777a776eca1cb4c923426949c997d8828ffffffffd87980d8799f581cf5808c2c990d86da54bfc97d89cee6efa20cd8461616359478d96b4c5820686db0c143a3a2cc19099d8909e315c4ed761a6ac5a3c5998c651d5e9d3cb253ffd8799fd87a80d8799f1a26ef03a4ff1adfaf40f4d87980ff1a001e8480d87a80ff';
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '700000000', { handle: 'alice' })],
        outputs: [{ address: MINSWAP_ORDER, lovelace: '657198244', assets: [], datum }],
      }),
    )!;
    expect(intent.subject).toMatchObject({ label: '$alice' });
    // Present tense: the order hasn't executed. Its settlement reads SWAPPED, later.
    expect(intent.verb).toBe('SWAPPING');
    // From the datum, not the 657.2 ₳ output — which also holds the batcher fee.
    expect(intent.amount).toEqual({ quantity: '653198244' });
    expect(intent.preposition).toBe('FOR');
    // Named, not counted: the datum's minimum is a slippage floor, not what will arrive.
    expect(intent.targets[0].amount).toEqual({ unit: 'WorldMobileTokenX' });
    expect(intent.via).toMatchObject({ label: 'MINSWAP' });
  });

  it('names ADA on the wanted side rather than rendering it as nothing', () => {
    // A real Minswap V2 order the other way round: NIGHT in, ADA out. ADA has no policy
    // and no asset name, so there's nothing to derive a label from — left unnamed it
    // falls through to the ADA-amount rendering and a missing quantity formats as
    // "0 ₳", which reads as swapping 40K NIGHT for nothing.
    const datum =
      'd8799fd8799f581c636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5affd8799fd8799f581c636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5affd8799fd8799fd8799f581ce39b5f40aa85fbc121a625d777a776eca1cb4c923426949c997d8828ffffffffd87980d8799fd8799f581c636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5affd8799fd8799fd8799f581ce39b5f40aa85fbc121a625d777a776eca1cb4c923426949c997d8828ffffffffd87980d8799f581cf5808c2c990d86da54bfc97d89cee6efa20cd8461616359478d96b4c5820686db0c143a3a2cc19099d8909e315c4ed761a6ac5a3c5998c651d5e9d3cb253ffd8799fd87980d8799f1a26ef03a4ff1adfaf40f4d87980ff1a001e8480d87a80ff';
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000')],
        outputs: [{ address: MINSWAP_ORDER, lovelace: '4000000', assets: [], datum }],
      }),
    )!;
    expect(intent.verb).toBe('SWAPPING');
    expect(intent.preposition).toBe('FOR');
    expect(intent.targets[0].amount).toEqual({ unit: 'ADA' });
  });

  it('reads a marketplace script as a trade', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '210000000')],
        outputs: [output(JPGSTORE, '200000000')],
      }),
    )!;
    expect(intent.verb).toBe('TRADED');
    expect(intent.via).toMatchObject({ label: 'JPG.STORE' });
  });

  it('still says SENT for a dApp script with no verb of its own', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '210000000')],
        outputs: [output(MINSWAP_POOL, '200000000')],
      }),
    )!;
    expect(intent.verb).toBe('SENT');
    expect(intent.targets[0].party).toMatchObject({ label: 'MINSWAP', kind: 'app' });
  });
});

// --- What the tx says about itself ---

describe('describeTx: CIP-20 tags', () => {
  it('reads the dApp and action out of the message', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '110000000', { handle: 'alice' })],
        outputs: [output(MINSWAP_ORDER, '100000000'), output(ALICE_A, '9000000')],
        metadata: message(['Minswap: Limit Order']),
      }),
    )!;
    expect(intent.subject).toMatchObject({ label: '$alice' });
    // The message says "Limit Order", so it isn't the SWAPPED the address alone implies.
    expect(intent.verb).toBe('ORDERED');
    expect(intent.amount).toEqual({ quantity: '100000000' });
    expect(intent.via).toMatchObject({ label: 'MINSWAP' });
    expect(intent.messageRead).toBe(true);
  });

  it('names a dApp whose script address is unknown to the registry', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '110000000')],
        outputs: [output(BOB, '100000000')],
        metadata: message(['Surf - Borrow - ADA / NIGHT']),
      }),
    )!;
    expect(intent.verb).toBe('BORROWED');
    expect(intent.via).toMatchObject({ label: 'SURF' });
  });

  it('reads a batcher settlement, which has no single sender', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '50000000'), input(BOB, '50000000')],
        outputs: [output(CAROL, '99000000')],
        metadata: message(['Minswap: Order Executed']),
      }),
    )!;
    // Structure alone gives up here; the message names the actor.
    expect(intent.subject).toMatchObject({ label: 'MINSWAP', kind: 'app' });
    expect(intent.verb).toBe('EXECUTED');
  });

  it('names the orders a batch settled, not just how many', () => {
    // Two real Minswap V2 order datums: ADA→WMTX and NIGHT→ADA. "EXECUTED 2 ORDERS"
    // counts them; the datums say which two.
    const adaForWmtx =
      'd8799fd8799f581c636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5affd8799fd8799f581c636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5affd8799fd8799fd8799f581ce39b5f40aa85fbc121a625d777a776eca1cb4c923426949c997d8828ffffffffd87980d8799fd8799f581c636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5affd8799fd8799fd8799f581ce39b5f40aa85fbc121a625d777a776eca1cb4c923426949c997d8828ffffffffd87980d8799f581cf5808c2c990d86da54bfc97d89cee6efa20cd8461616359478d96b4c5820686db0c143a3a2cc19099d8909e315c4ed761a6ac5a3c5998c651d5e9d3cb253ffd8799fd87a80d8799f1a26ef03a4ff1adfaf40f4d87980ff1a001e8480d87a80ff';
    const nightForAda = adaForWmtx.replace('ffd8799fd87a80d8799f', 'ffd8799fd87980d8799f');
    const intent = describeTx(
      tx({
        // A batcher's inputs: two order UTXOs and its own funding, so no single sender.
        inputs: [
          {
            tx_hash: '00'.repeat(32),
            index: 0,
            address: MINSWAP_ORDER,
            lovelace: '4000000',
            assets: [],
            datum: adaForWmtx,
          },
          {
            tx_hash: '11'.repeat(32),
            index: 0,
            address: MINSWAP_ORDER,
            lovelace: '4000000',
            // A token order's UTXO holds exactly the token being swapped, already scaled
            // by the server — which is where the amount on this line comes from.
            assets: [asset('asset1wmtx0000000000000000000000000000000', '40000')],
            datum: nightForAda,
          },
          input(BOB, '50000000'),
        ],
        outputs: [output(CAROL, '30000000'), output(ALICE_A, '20000000')],
        metadata: message(['Minswap: Order Executed']),
      }),
    )!;
    expect(intent.subject).toMatchObject({ label: 'MINSWAP' });
    expect(intent.verb).toBe('EXECUTED');
    // Each order says how much went in — "2 ORDERS" said neither which nor how much.
    // What came back is left out: one pool movement covers both orders, so splitting it
    // between them would be a guess.
    expect(intent.targets.map((t) => t.amount?.unit)).toEqual([
      '653 ₳ → WorldMobileTokenX',
      '40k WorldMobileTokenX → ADA',
    ]);
  });

  it('says USED when the dApp names itself but no action we have a word for', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000')],
        outputs: [output(BOB, '9000000')],
        metadata: message(['Minswap: MasterChef']),
      }),
    )!;
    expect(intent.verb).toBe('USED');
    expect(intent.via).toMatchObject({ label: 'MINSWAP' });
  });

  it('does not name the dApp twice when it is the one acting', () => {
    const intent = describeTx(
      tx({
        inputs: [input(MINSWAP_ORDER, '5000000')],
        outputs: [output(MINSWAP_ORDER, '4800000')],
        metadata: message(['Minswap: Aggregator Cancel Order']),
      }),
    )!;
    expect(intent.subject).toMatchObject({ label: 'MINSWAP' });
    expect(intent.verb).toBe('CANCELLED');
    expect(intent.via).toBeUndefined();
    // Nothing left the wallet, so the tx's own output total is the only number there is.
    expect(intent.amount).toEqual({ quantity: '4800000' });
  });

  it('leaves a human memo to the ordinary reading', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000')],
        outputs: [output(BOB, '9000000')],
        metadata: message(['thanks for lunch']),
      }),
    )!;
    expect(intent.verb).toBe('SENT');
    expect(intent.messageRead).toBeUndefined();
  });
});

// --- Mints and burns ---

describe('describeTx: mints', () => {
  it('reads a mint that lands back in the minters own wallet', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000', { handle: 'alice' })],
        outputs: [output(ALICE_A, '9500000', [asset('asset1nft')])],
        annotations: [{ kind: 'mint', minted: 1, burned: 0, created: [asset('asset1nft')], policies: ['aa'] }],
      }),
    )!;
    expect(intent.subject).toMatchObject({ label: '$alice' });
    expect(intent.verb).toBe('MINTED');
    expect(intent.assets).toEqual([asset('asset1nft')]);
    // Without the annotation this output is change, and the tx would read as MOVED.
    expect(intent.amount).toBeUndefined();
  });

  it('names the recipient when the mint goes straight to someone else', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000')],
        outputs: [output(BOB, '1500000', [asset('asset1nft')])],
        annotations: [{ kind: 'mint', minted: 1, burned: 0, created: [asset('asset1nft')], policies: ['aa'] }],
      }),
    )!;
    expect(intent.verb).toBe('MINTED');
    expect(intent.preposition).toBe('TO');
    expect(intent.targets[0].party).toMatchObject({ id: BOB });
  });

  it('reads a burn, which leaves nothing in the outputs', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000')],
        outputs: [output(ALICE_A, '9500000')],
        annotations: [{ kind: 'mint', minted: 0, burned: 3, policies: ['aa'] }],
      }),
    )!;
    expect(intent.verb).toBe('BURNED');
    expect(intent.amount).toEqual({ quantity: '3', unit: 'TOKENS' });
  });

  it('counts one minted token in the singular', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000')],
        outputs: [output(ALICE_A, '9500000')],
        annotations: [{ kind: 'mint', minted: 1, burned: 0, created: [], policies: ['aa'] }],
      }),
    )!;
    expect(intent.amount).toEqual({ quantity: '1', unit: 'TOKEN' });
  });

  it('counts the assets when there are more than the server sent fingerprints for', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000')],
        outputs: [output(ALICE_A, '9500000')],
        annotations: [{ kind: 'mint', minted: 500, burned: 0, created: [], policies: ['aa'] }],
      }),
    )!;
    expect(intent.amount).toEqual({ quantity: '500', unit: 'TOKENS' });
  });
});

// --- Withdrawals ---

describe('describeTx: withdrawals', () => {
  it('reads a rewards withdrawal', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000', { handle: 'alice' }), withdrawal(ALICE_STAKE, '12400000')],
        outputs: [output(ALICE_A, '22230000')],
      }),
    )!;
    expect(intent.subject).toMatchObject({ label: '$alice' });
    expect(intent.verb).toBe('WITHDREW');
    expect(intent.amount).toEqual({ quantity: '12400000' });
  });

  it('reads a withdrawal that also pays someone as a payment', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000'), withdrawal(ALICE_STAKE, '12400000')],
        outputs: [output(BOB, '20000000'), output(ALICE_A, '2230000')],
      }),
    )!;
    expect(intent.verb).toBe('SENT');
    expect(intent.targets[0].party).toMatchObject({ id: BOB });
  });
});

// --- Delegations ---

describe('describeTx: delegations', () => {
  it('reads a pool delegation', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000', { handle: 'alice' })],
        outputs: [output(ALICE_A, '9800000')],
        delegations: [delegation({ to_pool_id: POOL_ID, to_ticker: 'SMAUG' })],
      }),
    )!;
    expect(intent.subject).toMatchObject({ label: '$alice', id: ALICE_STAKE });
    expect(intent.verb).toBe('DELEGATED');
    expect(intent.amount).toEqual({ quantity: '50000000000' });
    expect(intent.preposition).toBe('TO');
    expect(intent.targets[0].party).toMatchObject({ label: 'SMAUG', kind: 'pool' });
  });

  it('names both targets when one tx delegates stake and vote', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000')],
        delegations: [
          delegation({ to_pool_id: POOL_ID, to_ticker: 'SMAUG', to_drep_id: DREP_ID, to_drep_name: 'Ada' }),
        ],
      }),
    )!;
    expect(intent.targets.map((t) => t.party?.kind)).toEqual(['pool', 'drep']);
  });

  it('reads a deregistration as leaving the pool', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000')],
        delegations: [delegation({ from_pool_id: POOL_ID, from_ticker: 'SMAUG' })],
      }),
    )!;
    expect(intent.verb).toBe('LEFT');
    expect(intent.targets[0].party).toMatchObject({ label: 'SMAUG', former: true });
  });

  it('falls back on a multi-account delegation batch', () => {
    expect(
      describeTx(
        tx({
          inputs: [input(ALICE_A, '10000000')],
          delegations: [
            delegation({ to_pool_id: POOL_ID }),
            delegation({ stake_address: 'stake1uyabc', to_pool_id: POOL_ID }),
          ],
        }),
      ),
    ).toBeNull();
  });
});

// --- Cases the raw view keeps ---

describe('describeTx: fallbacks', () => {
  it('leaves a governance vote to its own rendering', () => {
    expect(
      describeTx(
        tx({
          inputs: [input(ALICE_A, '10000000')],
          outputs: [output(ALICE_A, '9800000')],
          votes: [{ voter_role: 'DRep', voter_id: DREP_ID, vote: 'Yes', action_tx_hash: 'ff', action_index: 0 }],
        }),
      ),
    ).toBeNull();
  });

  it('leaves an oracle update to its own rendering', () => {
    expect(
      describeTx(
        tx({
          inputs: [input(ALICE_A, '10000000')],
          outputs: [output(ALICE_A, '9800000')],
          annotations: [{ kind: 'oracle', source: 'Aegis' }],
        }),
      ),
    ).toBeNull();
  });

  it('still says something when no input address resolved', () => {
    const intent = describeTx(
      tx({ inputs: [input(null as unknown as string, '0')], outputs: [output(BOB, '1000000')] }),
    )!;
    expect(intent.verb).toBe('SENT');
    expect(intent.targets[0].party).toMatchObject({ id: BOB });
  });
});
