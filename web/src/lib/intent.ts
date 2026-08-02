/**
 * Read a transaction as a sentence.
 *
 * A tx tile is 108px wide, so the sentence renders as a stack of short lines —
 * subject, verb, amount, preposition, object — and this module produces that stack
 * without touching the DOM. Everything here is derived from what a `BlockTx` already
 * carries, so it stays a pure function of the event and is tested as one.
 *
 *     $bob            subject
 *     SENT            verb
 *     1,234 ₳         amount     ← the loud line
 *     TO              preposition
 *     $alice          target
 *
 * Every transaction gets a sentence. Where the shape is unusual the sentence gets less
 * specific — a tx several wallets funded is subjectless in all but their number — but it
 * never falls back to a list of raw addresses, which said strictly less than even the
 * vaguest sentence does. The one exception is a tx with a purpose-built rendering of its
 * own: `describeTx` returns null for governance votes and oracle updates, which
 * `Transaction.svelte` draws its own way rather than as prose.
 *
 * Being less specific is the escape hatch, never being wrong: a confident wrong sentence
 * is worse than a vague true one.
 */
import type { AssetInfo, BlockTx, DelegationInfo, MintInfo, TxInput, TxOutputInfo } from './types';
import { nonChangeOutputs } from './change';
import { paymentIsScript, stakeAddressOf } from './bech32';
import { dappForAddress, dappForPolicy, isDex, type Dapp } from './dapps';
import { parseMessage, type TaggedAction } from './cip20';
import { messageLines, metadataLines } from './metadata';
import { readSettlement, type Side } from './settlement';
import { isAda, readOrder, type OrderAsset, type SwapOrder } from './dexOrder';
import { formatAdaCompact, formatCount, formatTicker } from './layout';

/** How a party's label was derived — drives its styling and colour. */
export type PartyKind = 'handle' | 'app' | 'pool' | 'drep' | 'address';

/** One named side of the sentence. */
export interface Party {
  /** What to render: `$bob`, `MINSWAP`, `SMAUG`, `addr1q8e…s2rm`. */
  label: string;
  /** Route to link to, or undefined to render as plain text (Byron, unresolved). */
  href?: string;
  /** The full id the label stands for — the colour seed, and the address feeds use it. */
  id?: string;
  kind: PartyKind;
  /** A delegation this party is leaving (rendered struck through). */
  former?: boolean;
}

/** A quantity in the sentence. `unit` absent means ADA and `quantity` is lovelace.
 *  `quantity` absent names an asset without one — a swap's wanted side, where the only
 *  figure available is a slippage floor rather than what will actually arrive. */
export interface Amount {
  quantity?: string;
  /** Token name for non-ADA amounts. */
  unit?: string;
  /** Asset fingerprint, so the amount can link to its asset page. */
  fingerprint?: string;
  /** The asset itself, when its art should be shown beside the figure. A swap is about
   *  two things, and the tokens are quicker to recognise as pictures than as tickers. */
  image?: AssetInfo;
}

/** One object of the sentence: who, and optionally how much went to them. */
export interface IntentTarget {
  amount?: Amount;
  assets?: AssetInfo[];
  party?: Party;
}

export interface Intent {
  /** Who acted. Absent when the tx has no identifiable actor. */
  subject?: Party;
  /** `SENT`, `WITHDREW`, `DELEGATED`, `SWAPPED`, … — always a single word. */
  verb: string;
  /** The loud line. Absent when each target carries its own amount instead. */
  amount?: Amount;
  /** Tokens moved alongside (or instead of) `amount`; rendered as thumbnails. */
  assets?: AssetInfo[];
  /** `TO`, `FOR`, `FROM` — absent when the verb takes no object. */
  preposition?: string;
  targets: IntentTarget[];
  /** Targets beyond the display cap, summarised as "+N more". */
  hiddenTargets: number;
  /** The dApp the action happened on, rendered as a trailing `ON <APP>`. */
  via?: Party;
  /** This sentence *is* the tx's CIP-20 message, read. The caller should not also
   *  render the raw message line, which would say the same thing twice. */
  messageRead?: boolean;
  /** The tx's own words, when they *are* the point — an on-chain note, a timestamp, an
   *  attestation. Rendered in place of an amount, since no value went anywhere. */
  note?: string[];
}

/** Kept short enough that `addr1q8e…s2rm` fits one line at 108px. */
const ADDRESS_HEAD = 8;
const ADDRESS_TAIL = 4;
/** Most targets to name before collapsing the rest into "+N more". */
const MAX_TARGETS = 4;
/**
 * An output at or below this much ADA alongside tokens is carrying the tokens, not
 * value: Cardano requires every UTXO to hold some ADA, and ~1.2-1.5 ₳ is what a
 * token-bearing output typically needs. Headlining it would make an NFT transfer read
 * as a 1.5 ₳ payment.
 */
const MIN_UTXO_DUST = 2_000_000n;

/** Middle-truncated address: identifying at both ends, one line at 108px. */
export function shortAddress(address: string): string {
  if (address.length <= ADDRESS_HEAD + ADDRESS_TAIL + 1) return address;
  return address.slice(0, ADDRESS_HEAD) + '…' + address.slice(-ADDRESS_TAIL);
}

/** Addresses that have a feed of their own; Byron and unresolved ones render as text. */
function hrefFor(address: string): string | undefined {
  return /^(addr1|addr_test1|stake1|stake_test1)/.test(address) ? '/' + address : undefined;
}

/**
 * Name an address: its ADA Handle if it has one, else the dApp that owns the script,
 * else a truncated form of the address itself.
 */
export function partyForAddress(address: string, handle?: string): Party {
  if (handle) {
    return { label: '$' + handle, href: hrefFor(address), id: address, kind: 'handle' };
  }
  const dapp = dappForAddress(address);
  if (dapp) {
    return { label: dapp.name.toUpperCase(), href: hrefFor(address), id: address, kind: 'app' };
  }
  return { label: shortAddress(address), href: hrefFor(address), id: address, kind: 'address' };
}

// --- Parties of a transaction ---

/** The withdrawal pseudo-inputs the server appends carry this index. */
const WITHDRAWAL_INDEX = -1;

function isWithdrawal(input: TxInput): boolean {
  return input.index === WITHDRAWAL_INDEX;
}

/**
 * The account an address belongs to. Payment addresses sharing a stake credential are
 * one wallet — a wallet routinely spends from many of its own payment addresses — so
 * this, not the address, is what tells "someone else" from "myself".
 *
 * A script address is its own account no matter whose stake credential it carries, and
 * that exception is load-bearing. Minswap V2 and the aggregators that route into it put
 * *the user's* stake credential on the order address, so the user keeps staking while
 * the order waits. Folding by stake credential alone read that order as the user's own
 * change and dropped it, which left a DexHunter trade headlined by the only output that
 * survived the filter — its 2 ₳ fee — instead of the swap.
 */
function walletOf(address: string): string {
  return paymentIsScript(address) ? address : (stakeAddressOf(address) ?? address);
}

interface Sender {
  party: Party;
  /** `walletOf` the funding address, for recognising its own outputs. */
  wallet: string;
}

/**
 * The single wallet that funded this tx, or null when more than one did.
 *
 * Among the wallet's payment addresses the one that contributed most is the one worth
 * naming — but the handle is looked for across all of them, since the address holding
 * the ADA Handle is rarely the address holding the funds.
 */
function soleSender(inputs: TxInput[]): Sender | null {
  const wallets = new Map<string, { input: TxInput; handle?: string; addresses: Set<string> }>();
  for (const input of inputs) {
    if (isWithdrawal(input) || !input.address) continue;
    const wallet = walletOf(input.address);
    const seen = wallets.get(wallet);
    if (!seen) {
      wallets.set(wallet, { input, handle: input.handle, addresses: new Set([input.address]) });
      continue;
    }
    seen.addresses.add(input.address);
    seen.handle ??= input.handle;
    if (BigInt(input.lovelace) > BigInt(seen.input.lovelace)) seen.input = input;
  }
  // A script UTXO the funder is taking back isn't a second funder. A cancelled DEX order
  // is spent from an address holding the canceller's own stake credential, so it can be
  // told apart from someone else's order — which a batcher spends, and which must keep
  // counting, since a batcher settling other people's orders has no single sender.
  const keys = [...wallets.keys()];
  const funders = keys.filter((key) => !paymentIsScript(key));
  if (funders.length === 1) {
    const account = stakeAddressOf(funders[0]) ?? funders[0];
    for (const key of keys) {
      if (paymentIsScript(key) && (stakeAddressOf(key) ?? key) === account) wallets.delete(key);
    }
  }
  if (wallets.size !== 1) return null;
  const [wallet, { input, handle, addresses }] = wallets.entries().next().value!;
  // Spending from several addresses of one account: the account is what they share, so
  // name that rather than picking one address to stand for the rest.
  const party = addresses.size === 1 ? partyForAddress(input.address!, handle) : partyForAddress(wallet, handle);
  return { party, wallet };
}

/** Sum the lovelace withdrawn from reward accounts by this tx. */
function withdrawn(inputs: TxInput[]): bigint {
  return inputs.filter(isWithdrawal).reduce((sum, i) => sum + BigInt(i.lovelace), 0n);
}

/** Outputs merged into one recipient, plus how that recipient should be named. */
interface Recipient {
  output: TxOutputInfo;
  party: Party;
}

/**
 * Merge outputs that go to the same *account* — not the same address.
 *
 * A wallet spreads a payment across several of its own payment addresses routinely, and
 * listing them separately says "paid three people" when one was paid. So outputs sharing
 * a stake credential collapse into one recipient with the values summed, named by the
 * account's ADA Handle if it has one, and by the stake address itself otherwise — which
 * is the thing they actually have in common. A recipient that is only one address keeps
 * being named by that address, since that's the more specific truth.
 */
function byRecipient(outputs: TxOutputInfo[]): Recipient[] {
  const merged = new Map<string, { output: TxOutputInfo; addresses: Set<string>; handle?: string }>();
  for (const output of outputs) {
    const account = walletOf(output.address);
    const seen = merged.get(account);
    if (!seen) {
      merged.set(account, {
        output: { ...output, assets: [...output.assets] },
        addresses: new Set([output.address]),
        handle: output.handle,
      });
      continue;
    }
    seen.output.lovelace = (BigInt(seen.output.lovelace) + BigInt(output.lovelace)).toString();
    seen.output.assets.push(...output.assets);
    seen.addresses.add(output.address);
    seen.handle ??= output.handle;
  }

  return [...merged.entries()].map(([account, { output, addresses, handle }]) => ({
    output,
    party: addresses.size === 1 ? partyForAddress(output.address, handle) : partyForAddress(account, handle),
  }));
}

// --- Verbs ---

/**
 * The verb for spending into a known dApp script. Only roles whose meaning is
 * unambiguous from the script alone get their own verb; the rest fall back to `SENT`,
 * which is never wrong.
 */
function verbForDapp(dapp: Dapp): string | null {
  switch (dapp.role) {
    case 'order':
      // Present tense: spending *into* an order script places an order, it doesn't
      // execute one. The settlement that fills it is a later transaction, and says
      // SWAPPED. This is the reading for orders whose datum we can't decode; the ones we
      // can say the same thing with both assets named.
      return isDex(dapp) ? 'SWAPPING' : 'ORDERED';
    case 'deposit':
      return 'DEPOSITED';
    case 'redeem':
      return 'REDEEMED';
    case 'lend':
      return 'LENT';
    case 'stake':
      return 'STAKED';
    case 'farm':
      return 'FARMED';
    case 'market':
      return 'TRADED';
    case 'vesting':
      return 'CLAIMED';
    default:
      return null;
  }
}

// --- Cases ---

/** The delegations worth describing — a registration alone carries no target. */
function visibleDelegations(tx: BlockTx): DelegationInfo[] {
  return (tx.delegations ?? []).filter((d) => d.from_pool_id || d.to_pool_id || d.from_drep_id || d.to_drep_id);
}

function poolParty(poolId: string, ticker: string | undefined, former: boolean): Party {
  return { label: formatTicker(ticker ?? poolId.slice(5, 10)), href: '/' + poolId, id: poolId, kind: 'pool', former };
}

function drepParty(drepId: string, name: string | undefined, former: boolean): Party {
  return { label: name ?? drepId.slice(5, 13), href: '/' + drepId, id: drepId, kind: 'drep', former };
}

/**
 * `$bob DELEGATED 50,000 ₳ TO SMAUG`, or `$bob LEFT SMAUG` when the tx only ends a
 * delegation. The amount is the account's live stake — what the delegation is worth to
 * the target, which is the number a reader cares about.
 */
function describeDelegation(tx: BlockTx, deleg: DelegationInfo): Intent {
  const targets: IntentTarget[] = [];
  if (deleg.to_pool_id) targets.push({ party: poolParty(deleg.to_pool_id, deleg.to_ticker, false) });
  if (deleg.to_drep_id) targets.push({ party: drepParty(deleg.to_drep_id, deleg.to_drep_name, false) });

  if (targets.length > 0) {
    return {
      subject: subjectForStake(tx, deleg.stake_address),
      verb: 'DELEGATED',
      amount: { quantity: deleg.live_stake },
      preposition: 'TO',
      targets,
      hiddenTargets: 0,
    };
  }
  // No new target: the tx ends the delegation(s) it had.
  if (deleg.from_pool_id) targets.push({ party: poolParty(deleg.from_pool_id, deleg.from_ticker, true) });
  if (deleg.from_drep_id) targets.push({ party: drepParty(deleg.from_drep_id, deleg.from_drep_name, true) });
  return {
    subject: subjectForStake(tx, deleg.stake_address),
    verb: 'LEFT',
    amount: { quantity: deleg.live_stake },
    targets,
    hiddenTargets: 0,
  };
}

/**
 * Name the delegating account by the handle of a payment address in the same tx that
 * shares its stake credential — `$bob` beats `stake1u9x…7k2q` — falling back to the
 * stake address itself.
 */
function subjectForStake(tx: BlockTx, stakeAddress: string): Party {
  for (const input of tx.inputs) {
    if (input.handle && input.address && stakeAddressOf(input.address) === stakeAddress) {
      return { label: '$' + input.handle, href: hrefFor(stakeAddress), id: stakeAddress, kind: 'handle' };
    }
  }
  return { label: shortAddress(stakeAddress), href: hrefFor(stakeAddress), id: stakeAddress, kind: 'address' };
}

/**
 * `$bob WITHDREW 12.4 ₳` — staking rewards moved out of a reward account. Falls back to
 * naming the reward account itself when the payment inputs don't identify one wallet.
 */
function describeWithdrawal(tx: BlockTx, amount: bigint, sender?: Party): Intent | null {
  const account = tx.inputs.find((i) => isWithdrawal(i) && i.address);
  const subject = sender ?? (account ? subjectForStake(tx, account.address!) : undefined);
  if (!subject) return null;
  return { subject, verb: 'WITHDREW', amount: { quantity: amount.toString() }, targets: [], hiddenTargets: 0 };
}

/** Build the target for one recipient, headlining tokens over min-UTXO dust. */
function targetFor({ output, party }: Recipient): IntentTarget {
  const lovelace = BigInt(output.lovelace);
  const carriesTokens = output.assets.length > 0;
  return {
    party,
    assets: carriesTokens ? output.assets : undefined,
    amount: carriesTokens && lovelace <= MIN_UTXO_DUST ? undefined : { quantity: output.lovelace },
  };
}

/**
 * A pending swap, read from the order's datum: `$bob SWAPPING 653.2 ₳ FOR 3,752.8 WMTX`.
 *
 * The transaction alone can't say this. It shows value going to a script and nothing
 * about what for, and even the amount is wrong if taken from the order UTXO, which also
 * holds the batcher fee and a deposit that come back. Only the datum has it.
 *
 * The present tense is load-bearing: this order hasn't executed. Its settlement arrives
 * in a later block and reads `SWAPPED`, with the amount that was actually filled rather
 * than the minimum asked for.
 */
function describePendingSwap(subject: Party, order: SwapOrder, output: TxOutputInfo, app: Party): Intent {
  // The wanted side is named but not counted. The datum's `minimumReceived` is a
  // slippage floor, not a forecast — the fill is nearly always better — so putting a
  // figure on it would claim a precision the order doesn't have. The settlement says
  // what actually arrived.
  const wanted = wantedName(order.want);
  return {
    subject,
    verb: 'SWAPPING',
    amount: offered(order, output),
    // No name for the wanted asset means no object: "FOR" followed by nothing reads as
    // an unfinished sentence, and an amount-less ADA unit would render as "0 ₳".
    preposition: wanted ? 'FOR' : undefined,
    targets: wanted ? [{ amount: { unit: wanted } }] : [],
    hiddenTargets: 0,
    via: app,
  };
}

/**
 * What to call the asset a swap wants.
 *
 * ADA has no policy and no asset name, so it has nothing to derive a label from and has
 * to be named outright — without this it falls through to the ADA-amount rendering and
 * a missing quantity formats as `0 ₳`, which reads as a swap for nothing.
 */
function wantedName(asset: OrderAsset): string | undefined {
  return isAda(asset) ? 'ADA' : assetTicker(asset);
}

/**
 * What the order puts in, exactly.
 *
 * ADA comes from the datum, since the order UTXO's lovelace also holds the batcher fee
 * and a deposit that come back. A token comes from the output instead: the UTXO holds
 * exactly the token being swapped — there's no fee taken in it — and the server has
 * already scaled it by the asset's decimals and named it, which the datum's raw integer
 * would need those decimals to do.
 */
function offered(order: SwapOrder, output: TxOutputInfo): Amount {
  if (isAda(order.give)) return { quantity: order.giveAmount.toString() };
  const asset = output.assets.length === 1 ? output.assets[0] : undefined;
  if (asset) return { quantity: asset.quantity, unit: asset.name, fingerprint: asset.fingerprint, image: asset };
  return { quantity: order.giveAmount.toString(), unit: assetTicker(order.give) };
}

/** CIP-67 label prefixes, as the server's `display_asset_name` strips them. */
const CIP67_LABELS = ['00000000', '00001070', '000643b0', '000de140', '0014df10', '001bc280'];

/**
 * A token's on-chain name, when it's readable text.
 *
 * The CIP-67 label has to come off first — it's four binary bytes that aren't part of
 * what the token is called, and leaving them on turns `PULSE` into `\ufffdPULSE`.
 */
function assetTicker(asset: OrderAsset): string | undefined {
  const label = CIP67_LABELS.find((l) => asset.name.startsWith(l));
  const hex = label ? asset.name.slice(label.length) : asset.name;
  const text = (hex.match(/../g) ?? []).map((b) => String.fromCharCode(parseInt(b, 16))).join('');
  return /^[\x20-\x7e]+$/.test(text) ? text : undefined;
}

/**
 * The one output that *is* the action, when this tx is an interaction with a dApp.
 *
 * A dApp interaction rarely has a single recipient: an order posted to a DEX comes with
 * a batcher fee to a separate address, and listing both as recipients buries the action
 * under its overheads. So the dApp output qualifies only when it's the dominant one —
 * worth more than everything else the tx pays out combined — which is what distinguishes
 * "swapped 73 ₳ on Minswap, 2 ₳ of it in fees" from "paid two different people".
 *
 * Null when no recipient is a dApp with a verb of its own, when several are (the tx does
 * more than one thing), or when the dApp's share doesn't dominate.
 */
function dappAction(recipients: Recipient[]): Recipient | null {
  const actions = recipients.filter((r) => {
    const dapp = dappForAddress(r.output.address);
    return dapp !== undefined && verbForDapp(dapp) !== null;
  });
  if (actions.length !== 1) return null;
  const action = actions[0];
  const rest = recipients.reduce((sum, r) => (r === action ? sum : sum + BigInt(r.output.lovelace)), 0n);
  // Tokens carry the value in a token order, where the ADA is only min-UTXO.
  return BigInt(action.output.lovelace) > rest || action.output.assets.length > 0 ? action : null;
}

/**
 * What a tx states about itself in its CIP-20 message, as a sentence.
 *
 * This outranks every structural signal, because it isn't inference: the dApp wrote
 * `"Minswap: Market Order"` into the tx to say exactly that. It also reaches the cases
 * structure can't — a batcher settling other people's orders has no single sender, and
 * a contract too new for any registry still names itself.
 *
 * The amount comes from the dApp output when there is one, else from what left the
 * wallet, so the loud line stays the number the reader cares about.
 */
function describeTagged(tag: TaggedAction, sender: Party | undefined, recipients: Recipient[], tx: BlockTx): Intent {
  const outputs = tx.outputs;
  const app: Party = { label: tag.app.toUpperCase(), kind: 'app' };
  const action = dappAction(recipients) ?? (recipients.length === 1 ? recipients[0] : null);
  const target = action ? targetFor(action) : null;

  if (sender) {
    // Nothing left the wallet — a cancellation, or a settlement that only returns funds
    // — so the tx's own output total is the only number there is to show.
    const moved = recipients.length > 0 ? sumLovelace(recipients.map((r) => r.output)) : sumLovelace(outputs);
    return {
      subject: sender,
      verb: tag.verb ?? 'USED',
      amount: target?.amount ?? { quantity: moved.toString() },
      assets: target?.assets,
      targets: [],
      hiddenTargets: 0,
      // The dApp spending its own script is already named as the subject; repeating it
      // as the venue would read "MINSWAP CANCELLED … ON MINSWAP".
      via: sender.label === app.label ? undefined : app,
      messageRead: true,
    };
  }
  // A settled order states the whole swap — both sides, exactly — in the pool's balance
  // change, and it belongs to the user who posted the order, not to the batcher that
  // happened to submit it.
  const settled = readSettlement(tx.inputs, tx.outputs, walletOf);
  if (settled) {
    return {
      subject: partyForAddress(settled.beneficiary.address, settled.beneficiary.handle),
      verb: tag.verb === 'EXECUTED' ? 'SWAPPED' : (tag.verb ?? 'SWAPPED'),
      amount: sideAmount(settled.gave),
      preposition: 'FOR',
      targets: [{ amount: sideAmount(settled.got) }],
      hiddenTargets: 0,
      via: app,
      messageRead: true,
    };
  }

  // No one wallet funded it — a batcher settling orders it holds, so the dApp is the
  // actor. Each order it spent carries a datum saying what that order asked for, which
  // is the difference between "2 orders" and which two. The tx's ADA total says nothing:
  // it's mostly liquidity pools rewritten and batcher change, not value anybody sent.
  const orders = settledOrders(tx.inputs);
  if (orders.length > 0) {
    return {
      subject: app,
      verb: tag.verb ?? 'USED',
      preposition: undefined,
      targets: orders.slice(0, MAX_TARGETS).map((order) => ({ amount: swapPair(order) })),
      hiddenTargets: Math.max(0, orders.length - MAX_TARGETS),
      messageRead: true,
    };
  }
  const counted = ordersSettled(tx.inputs);
  return {
    subject: app,
    verb: tag.verb ?? 'USED',
    amount: counted > 0 ? { quantity: String(counted), unit: counted === 1 ? 'ORDER' : 'ORDERS' } : undefined,
    targets: [],
    hiddenTargets: 0,
    messageRead: true,
  };
}

/** A settled order together with the UTXO it was posted in. */
interface SettledOrder {
  order: SwapOrder;
  utxo: TxInput;
}

/**
 * The orders a batch settled, read from the datums of the UTXOs it spent.
 *
 * A settlement's inputs *are* the orders — each one is a user's order UTXO, and its
 * datum says what that user asked for. Without them a batch can only be counted; with
 * them it can be read.
 */
function settledOrders(inputs: TxInput[]): SettledOrder[] {
  return inputs.flatMap((utxo) => {
    if (!utxo.address || !utxo.datum) return [];
    const order = readOrder(dappForAddress(utxo.address)?.name, utxo.datum);
    return order ? [{ order, utxo }] : [];
  });
}

/**
 * One settled order as a single line: `40K NIGHT → ADA`.
 *
 * The amount is the one going *in*, which the order states exactly. What came back isn't
 * stated: a batch settles several unrelated orders against one pool movement, so the
 * aggregate can't be split between them, and the payout UTXO mixes the fill with the
 * deposit the protocol returns. `readSettlement` reports both sides exactly when the
 * batch holds a single order and that ambiguity doesn't arise.
 */
function swapPair({ order, utxo }: SettledOrder): Amount {
  const want = wantedName(order.want) ?? '?';
  return { unit: `${offeredText(order, utxo)} → ${want}` };
}

/** The going-in side of an order, rendered compactly enough for a 108px line. */
function offeredText(order: SwapOrder, utxo: { lovelace: string; assets?: AssetInfo[] }): string {
  if (isAda(order.give)) return formatAdaCompact(order.giveAmount.toString());
  // A token order's UTXO holds exactly the token being swapped — no fee is taken in it —
  // and the server has already scaled it by the asset's decimals and named it, which the
  // datum's raw integer would need those decimals to do.
  const assets = utxo.assets ?? [];
  const asset = assets.length === 1 ? assets[0] : undefined;
  const name = assetTicker(order.give) ?? asset?.name ?? '?';
  if (!asset) return name;
  return `${formatCount(Number(asset.quantity))} ${asset.name ?? name}`;
}

/**
 * Orders a batch settled: one per order UTXO it spent.
 *
 * Counted from the inputs rather than the payouts because the inputs are exact — every
 * order the batcher consumed is an input from the app's order script, while the outputs
 * mix user payouts with pool UTXOs and the batcher's own change. Zero when the order
 * script isn't one we can name, which is the honest answer rather than a guess.
 */
function ordersSettled(inputs: TxInput[]): number {
  return inputs.filter((i) => i.address && dappForAddress(i.address)?.role === 'order').length;
}

/** One side of a settled swap as a sentence amount: ADA carries no unit. */
function sideAmount(side: Side): Amount {
  if (!side.asset) return { quantity: side.quantity };
  return { quantity: side.quantity, unit: side.asset.name, fingerprint: side.asset.fingerprint, image: side.asset };
}

function sumLovelace(outputs: TxOutputInfo[]): bigint {
  return outputs.reduce((sum, o) => sum + BigInt(o.lovelace), 0n);
}

/**
 * The default reading: value leaving one wallet for others. A single recipient gets the
 * full sentence with the amount on its own loud line; several recipients keep their
 * amounts next to their names, since there's no one number to headline.
 */
function describeTransfer(
  subject: Party | undefined,
  recipients: Recipient[],
  outputs: TxOutputInfo[],
  message?: string[],
): Intent {
  if (recipients.length === 0) {
    // Everything came back to the sender. If the tx also carries metadata, that metadata
    // is why it exists — nothing was paid to anyone, and leading with the ADA describes
    // the mechanism while hiding the purpose. An on-chain timestamp or attestation moves
    // value only because a transaction has to.
    if (message?.length) {
      return { subject, verb: 'WROTE', note: message, targets: [], hiddenTargets: 0, messageRead: true };
    }
    // Otherwise it really is just a wallet reorganising its own UTXOs.
    const moved = outputs.reduce((sum, o) => sum + BigInt(o.lovelace), 0n);
    return { subject, verb: 'MOVED', amount: { quantity: moved.toString() }, targets: [], hiddenTargets: 0 };
  }

  const action = dappAction(recipients);
  if (action) {
    // The dApp is the venue, not a recipient: `$bob SWAPPED 100 ₳ ON MINSWAP`.
    const target = targetFor(action);
    return {
      subject,
      verb: verbForDapp(dappForAddress(action.output.address)!)!,
      amount: target.amount,
      assets: target.assets,
      targets: [],
      hiddenTargets: 0,
      via: target.party,
    };
  }

  if (recipients.length === 1) {
    const target = targetFor(recipients[0]);
    return {
      subject,
      verb: 'SENT',
      amount: target.amount,
      assets: target.assets,
      preposition: 'TO',
      targets: [{ party: target.party }],
      hiddenTargets: 0,
    };
  }

  // Several recipients share one total rather than each carrying their own amount.
  // Stacked vertically in a 108px column an amount above a name doesn't read as belonging
  // to it — "SENT / TO / 572 ₳ / DdzFF…" parses as sending *to* the amount. The sum is
  // unambiguous, and the names still say who got it.
  const sorted = [...recipients].sort((a, b) => (BigInt(b.output.lovelace) > BigInt(a.output.lovelace) ? 1 : -1));
  const total = sumLovelace(sorted.map((r) => r.output));
  const assets = sorted.flatMap((r) => r.output.assets);
  // Every output has to carry min-UTXO, so the dust floor scales with how many there are.
  const carryingTokens = assets.length > 0 && total <= MIN_UTXO_DUST * BigInt(sorted.length);
  return {
    subject,
    verb: 'SENT',
    amount: carryingTokens ? undefined : { quantity: total.toString() },
    assets: assets.length > 0 ? assets : undefined,
    preposition: 'TO',
    targets: sorted.slice(0, MAX_TARGETS).map((r) => ({ party: r.party })),
    hiddenTargets: Math.max(0, sorted.length - MAX_TARGETS),
  };
}

function tokenUnit(count: number): string {
  return count === 1 ? 'TOKEN' : 'TOKENS';
}

/**
 * `$bob MINTED <thumbnails> ON JPG.STORE`, or `$bob BURNED 3 TOKENS`.
 *
 * A mint is otherwise invisible: the new token sits in an output looking exactly like
 * one that was transferred, and since it usually lands back in the minter's own wallet
 * the tx would read as `MOVED`. A burn is worse — nothing in the outputs records it.
 */
function describeMint(mint: MintInfo, subject: Party | undefined, recipients: Recipient[]): Intent {
  const app = mint.policies.map(dappForPolicy).find((d) => d !== undefined);
  const via: Party | undefined = app ? { label: app.name.toUpperCase(), kind: 'app' } : undefined;

  if (mint.minted === 0) {
    // The assets are gone from the chain, but the annotation still carries their names
    // and art — "BURNED 1 TOKEN" would say nothing a reader can act on.
    return {
      subject,
      verb: 'BURNED',
      amount: mint.destroyed?.length ? undefined : { quantity: String(mint.burned), unit: tokenUnit(mint.burned) },
      assets: mint.destroyed?.length ? mint.destroyed : undefined,
      targets: [],
      hiddenTargets: 0,
      via,
    };
  }

  const assets = mint.created ?? [];
  // Minting straight to someone else is worth saying; minting to yourself isn't.
  const target = recipients.length === 1 ? recipients[0].party : undefined;
  return {
    subject,
    verb: 'MINTED',
    amount: assets.length > 0 ? undefined : { quantity: String(mint.minted), unit: tokenUnit(mint.minted) },
    assets: assets.length > 0 ? assets : undefined,
    preposition: target ? 'TO' : undefined,
    targets: target ? [{ party: target }] : [],
    hiddenTargets: 0,
    via: target ? undefined : via,
  };
}

/**
 * The sentence for `tx`, or null when it can't be stated plainly — the caller then
 * renders the raw inputs and outputs.
 */
export function describeTx(tx: BlockTx): Intent | null {
  // Votes, Catalyst registrations and oracle updates each have a purpose-built
  // rendering already; a generic sentence would only bury them.
  const annotations = tx.annotations ?? [];
  const mint = annotations.find((a) => a.kind === 'mint');
  if (tx.votes?.length || tx.catalyst || annotations.some((a) => a.kind !== 'mint')) return null;

  const delegations = visibleDelegations(tx);
  if (delegations.length === 1) return describeDelegation(tx, delegations[0]);
  if (delegations.length > 1) return null; // a batch: no single actor to name

  const sender = soleSender(tx.inputs);
  const recipients = outsideRecipients(tx, sender?.wallet);

  if (mint) return describeMint(mint, sender?.party, recipients);

  const rewards = withdrawn(tx.inputs);
  if (rewards > 0n && recipients.length === 0) return describeWithdrawal(tx, rewards, sender?.party);

  // An order's datum outranks even the tx's own message: "Minswap: Market Order" says a
  // swap was placed, the datum says which one, for how much, and against what.
  if (sender) {
    for (const recipient of recipients) {
      const dapp = dappForAddress(recipient.output.address);
      const order = readOrder(dapp?.name, recipient.output.datum);
      if (order) return describePendingSwap(sender.party, order, recipient.output, recipient.party);
    }
  }

  // What the tx says about itself beats anything inferred from its shape.
  const tag = parseMessage(messageLines(tx.metadata));
  if (tag) return describeTagged(tag, sender?.party, recipients, tx);

  if (!sender) return describeShared(tx, recipients);
  return describeTransfer(sender.party, recipients, tx.outputs, metadataLines(tx.metadata));
}

/**
 * Several wallets funded this tx, so "who sent" has no single answer.
 *
 * It still did something, and how many wallets acted is a true and useful subject —
 * co-signed payments, exchange sweeps and collaborative txs all land here. This used to
 * fall back to a list of raw addresses, which said strictly less than the sentence does.
 */
function describeShared(tx: BlockTx, recipients: Recipient[]): Intent {
  const funders = new Set(tx.inputs.filter((i) => !isWithdrawal(i) && i.address).map((i) => walletOf(i.address!)));
  // No funder resolved at all leaves the sentence subjectless rather than claiming
  // "0 WALLETS" — the verb and the amount are still true without one.
  const subject: Party | undefined = funders.size > 0 ? { label: `${funders.size} WALLETS`, kind: 'app' } : undefined;
  return describeTransfer(subject, recipients, tx.outputs, metadataLines(tx.metadata));
}

/**
 * The recipients that are somebody else. `nonChangeOutputs` reads an output exceeding
 * what its own address put in as a receipt, which is right for a feed but not for a
 * sentence: value landing on another payment address of the sender's own account hasn't
 * been sent anywhere.
 */
function outsideRecipients(tx: BlockTx, wallet?: string): Recipient[] {
  const merged = byRecipient(nonChangeOutputs(tx.inputs, tx.outputs));
  return wallet === undefined ? merged : merged.filter((r) => walletOf(r.output.address) !== wallet);
}
