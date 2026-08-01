import { describe, it, expect } from 'vitest';
import type { AssetInfo, BlockTx, DelegationInfo, TxInput, TxOutputInfo } from './types';
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

  it('gives up when two wallets funded the tx', () => {
    expect(
      describeTx(
        tx({
          inputs: [input(ALICE_A, '4000000'), input(BOB, '9000000')],
          outputs: [output(CAROL, '12000000')],
        }),
      ),
    ).toBeNull();
  });

  it('keeps per-recipient amounts when there are several', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '20000000')],
        outputs: [output(BOB, '5000000'), output(CAROL, '9000000')],
      }),
    )!;
    expect(intent.amount).toBeUndefined();
    expect(intent.targets.map((t) => t.amount?.quantity)).toEqual(['9000000', '5000000']);
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
  it('reads an order posted to a DEX as a swap on it', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '110000000', { handle: 'alice' })],
        outputs: [output(MINSWAP_ORDER, '100000000'), output(ALICE_A, '9000000')],
      }),
    )!;
    expect(intent.verb).toBe('SWAPPED');
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
    expect(intent.verb).toBe('SWAPPED');
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
        message: ['Minswap: Limit Order'],
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
        message: ['Surf - Borrow - ADA / NIGHT'],
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
        message: ['Minswap: Order Executed'],
      }),
    )!;
    // Structure alone gives up here; the message names the actor.
    expect(intent.subject).toMatchObject({ label: 'MINSWAP', kind: 'app' });
    expect(intent.verb).toBe('EXECUTED');
  });

  it('says USED when the dApp names itself but no action we have a word for', () => {
    const intent = describeTx(
      tx({
        inputs: [input(ALICE_A, '10000000')],
        outputs: [output(BOB, '9000000')],
        message: ['Minswap: MasterChef'],
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
        message: ['Minswap: Aggregator Cancel Order'],
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
        message: ['thanks for lunch'],
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

  it('gives up when no input address resolved', () => {
    expect(
      describeTx(tx({ inputs: [input(null as unknown as string, '0')], outputs: [output(BOB, '1000000')] })),
    ).toBeNull();
  });
});
