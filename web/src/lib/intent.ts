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
import {
  addQuantities,
  formatScaled,
  nonChangeOutputs,
  parseQuantity,
  subtractQuantities,
  type ScaledQty,
} from './change';
import { paymentIsScript, stakeAddressOf } from './bech32';
import { dappForAddress, dappForPolicy, isDex, type Dapp } from './dapps';
import { parseMessage, type TaggedAction } from './cip20';
import { messageLines, metadataLines } from './metadata';
import { readSettlement, type Side } from './settlement';
import { isAda, readOrder, type OrderAsset, type SwapOrder } from './dexOrder';
import { formatAdaCompact, formatCount, formatTicker } from './layout';
import { assetLabel } from './assetName';

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
  /** Who acted, when it was several accounts that can't be folded into one. Rendered in
   *  place of `subject`: naming them is the point, since "3 wallets" says nothing a
   *  reader can follow. */
  subjects?: Party[];
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
/** Most funding accounts to name when several paid. Beyond this the tile is all subject
 *  and no sentence. */
const MAX_SUBJECTS = 3;
/**
 * An output at or below this much ADA alongside tokens is carrying the tokens, not
 * value: Cardano requires every UTXO to hold some ADA, and ~1.2-1.5 ₳ is what a
 * token-bearing output typically needs. Headlining it would make an NFT transfer read
 * as a 1.5 ₳ payment.
 */
const MIN_UTXO_DUST = 2_000_000n;

/**
 * A dApp's name as a tile label.
 *
 * "Protocol" and "Finance" are corporate suffixes rather than what anyone calls the
 * project, and at 108px they cost the name itself — "Splash Protocol" ellipsised to
 * "SPLASH PROTOCO…", which is strictly worse than "SPLASH".
 */
const APP_SUFFIX = /\s+(protocol|finance)$/i;

export function appLabel(name: string): string {
  return name.replace(APP_SUFFIX, '').toUpperCase();
}

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
    return { label: appLabel(dapp.name), href: hrefFor(address), id: address, kind: 'app' };
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
  // A script UTXO isn't a party. Somebody spent it — a user reclaiming a cancelled DEX
  // order, a batcher settling one, a vault being drawn on — and that somebody is the
  // wallet that signed and paid. Counting the script as a second funder left all of them
  // subjectless: a Surf leveraged borrow read as "SURF REFUNDED", naming the protocol and
  // nothing else. A batch of *other people's* orders is still attributed to the protocol,
  // because `describeSettlement` recognises it structurally before this is consulted.
  const keys = [...wallets.keys()];
  const funders = keys.filter((key) => !paymentIsScript(key));
  if (funders.length === 1) {
    for (const key of keys) if (paymentIsScript(key)) wallets.delete(key);
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
    // The tx's own "Minswap: Market Order" says exactly this and less precisely, so it
    // shouldn't also be printed above the sentence that replaced it.
    messageRead: true,
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
  if (isAda(order.give)) return { quantity: (order.giveAmount ?? BigInt(output.lovelace)).toString() };
  const asset = output.assets.length === 1 ? output.assets[0] : undefined;
  if (asset) return { quantity: asset.quantity, unit: asset.name, fingerprint: asset.fingerprint, image: asset };
  return { quantity: order.giveAmount?.toString(), unit: assetTicker(order.give) };
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
function describeTagged(
  tag: TaggedAction,
  sender: Party | undefined,
  recipients: Recipient[],
  tx: BlockTx,
  senderWallet?: string,
): Intent {
  const app: Party = { label: appLabel(tag.app), kind: 'app' };

  // A batcher settling other people's orders funds the tx from its own wallet, so it
  // *has* a sole sender — but the swap belongs to whoever posted the order, not to the
  // batcher. Recognised structurally, before the sender is considered.
  const settled = describeSettlement(tx, app, tag.verb, senderWallet);
  if (settled) return settled;

  const action = dappAction(recipients) ?? (recipients.length === 1 ? recipients[0] : null);
  const target = action ? targetFor(action) : null;

  if (sender) {
    // Nothing went to anyone else, so the tx's outputs are the sender's own funds coming
    // back. Headlining that total states the size of their wallet, not of what they did:
    // a DexHunter cancellation whose order lives off-chain moves one UTXO to itself, and
    // read as "CANCELLED 93,619 ₳" — a figure with nothing to do with the cancelled order.
    const moved =
      recipients.length > 0 ? { quantity: sumLovelace(recipients.map((r) => r.output)).toString() } : undefined;
    return {
      subject: sender,
      verb: tag.verb ?? 'USED',
      amount: target?.amount ?? moved,
      assets: target?.assets,
      targets: [],
      hiddenTargets: 0,
      // The dApp spending its own script is already named as the subject; repeating it
      // as the venue would read "MINSWAP CANCELLED … ON MINSWAP".
      via: sender.label === app.label ? undefined : app,
      messageRead: true,
    };
  }
  return {
    subject: app,
    verb: tag.verb ?? 'USED',
    targets: [],
    hiddenTargets: 0,
    messageRead: true,
  };
}

/**
 * A batcher settling orders it holds, or null when this tx isn't one.
 *
 * This can't wait for the tx to name itself. SundaeSwap, WingRiders and VyFi settle
 * without writing a CIP-20 message at all, and read as "N WALLETS SENT …" — true, but it
 * buries the swap the transaction exists to perform. What identifies a settlement is
 * structural and always there: it spends UTXOs from a DEX's order script.
 *
 * The batcher is not the actor worth naming when a single order can be attributed to the
 * user who posted it — the swap is theirs, and the batcher only submitted it.
 */
function describeSettlement(tx: BlockTx, app?: Party, verb?: string, ownWallet?: string): Intent | null {
  const orderInputs = tx.inputs.filter((i) => {
    // A withdrawal is a pseudo-input carrying a reward address, not a UTXO anyone can
    // post an order in. Reward addresses match a dApp whenever they share its stake
    // credential, so without this a plain withdrawal reads as a settled batch.
    if (isWithdrawal(i) || !i.address) return false;
    const dapp = dappForAddress(i.address);
    // The registry's role label is a naming convention, and several protocols don't
    // follow it — CSwap's order scripts carry no role at all. A datum that decodes as an
    // order is the stronger evidence, so either qualifies.
    if (dapp?.role !== 'order' && !readOrder(dapp?.name, i.datum, i)) return false;
    // ...but only an exchange settles swaps. The role comes from matching words in the
    // script's registry name, and Liqwid's *batch* script matched "batch" — so a lending
    // protocol reorganising its own reserves read as "LIQWID SETTLED 1 ORDER".
    if (!dapp || !isDex(dapp)) return false;
    // An order the funder is taking back is a cancellation, not a batch being settled.
    return ownWallet === undefined || (stakeAddressOf(i.address) ?? i.address) !== ownWallet;
  });
  if (orderInputs.length === 0) return null;
  const venue = app ?? appParty(dappForAddress(orderInputs[0].address!)?.name);

  // One order against one pool states the whole swap — both sides, exactly — in the
  // pool's balance change, and it belongs to whoever posted the order.
  const settled = readSettlement(tx.inputs, tx.outputs, walletOf);
  if (settled) {
    return {
      subject: partyForAddress(settled.beneficiary.address, settled.beneficiary.handle),
      // "EXECUTED" is what the protocol calls it; "SWAPPED" is what happened.
      verb: verb === 'EXECUTED' || verb === undefined ? 'SWAPPED' : verb,
      amount: sideAmount(settled.gave),
      preposition: 'FOR',
      targets: [{ amount: sideAmount(settled.got) }],
      hiddenTargets: 0,
      via: venue,
      messageRead: true,
    };
  }

  // Several orders share one pool movement, so no one of them can claim it. Each order's
  // datum still says what it asked for, which is the difference between "2 orders" and
  // which two. The tx's ADA total says nothing: it's mostly liquidity pools rewritten and
  // batcher change, not value anybody sent.
  const orders = settledOrders(tx.inputs);
  if (orders.length > 0) {
    return {
      subject: venue,
      verb: verb ?? 'SETTLED',
      targets: orders.slice(0, MAX_TARGETS).map((order) => ({ amount: swapPair(order) })),
      hiddenTargets: Math.max(0, orders.length - MAX_TARGETS),
      messageRead: true,
    };
  }

  // No datum we can read — a protocol whose order shape isn't decoded yet. Counting them
  // is the honest remainder.
  const counted = orderInputs.length;
  return {
    subject: venue,
    verb: verb ?? 'SETTLED',
    amount: { quantity: String(counted), unit: counted === 1 ? 'ORDER' : 'ORDERS' },
    targets: [],
    hiddenTargets: 0,
    messageRead: true,
  };
}

/** A dApp as the sentence's actor or venue. */
function appParty(name: string | undefined): Party | undefined {
  return name ? { label: appLabel(name), kind: 'app' } : undefined;
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
    if (isWithdrawal(utxo) || !utxo.address || !utxo.datum) return [];
    const order = readOrder(dappForAddress(utxo.address)?.name, utxo.datum, utxo);
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
  if (isAda(order.give)) return formatAdaCompact((order.giveAmount ?? BigInt(utxo.lovelace)).toString());
  // A token order's UTXO holds exactly the token being swapped — no fee is taken in it —
  // and the server has already scaled it by the asset's decimals and named it, which the
  // datum's raw integer would need those decimals to do.
  const assets = utxo.assets ?? [];
  const asset = assets.length === 1 ? assets[0] : undefined;
  const name = assetTicker(order.give) ?? asset?.name ?? '?';
  if (!asset) return name;
  return `${formatCount(Number(asset.quantity))} ${asset.name ?? name}`;
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
function describeMint(
  mint: MintInfo,
  subject: Party | undefined,
  recipients: Recipient[],
  tag: TaggedAction | null,
): Intent {
  const app = mint.policies.map(dappForPolicy).find((d) => d !== undefined);
  // The minting policy names the dApp when it's a known one; otherwise the tx's own
  // message does. Either way, saying it in the sentence means the raw message line
  // shouldn't also be printed above it.
  const via: Party | undefined = app ? appParty(app.name) : appParty(tag?.app);
  const messageRead = tag !== null;

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
      messageRead,
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
    messageRead,
  };
}

/**
 * The sentence for `tx`, or null when it can't be stated plainly — the caller then
 * renders the raw inputs and outputs.
 */
export function describeTx(tx: BlockTx): Intent | null {
  const intent = readTx(tx);
  return intent === null ? null : headlineAsset(intent);
}

function readTx(tx: BlockTx): Intent | null {
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

  // What the tx says about itself, read once and used wherever it helps.
  const tag = parseMessage(messageLines(tx.metadata));

  if (mint) return describeMint(mint, sender?.party, recipients, tag);

  const rewards = withdrawn(tx.inputs);
  if (rewards > 0n && recipients.length === 0) return describeWithdrawal(tx, rewards, sender?.party);

  // An order's datum outranks even the tx's own message: "Minswap: Market Order" says a
  // swap was placed, the datum says which one, for how much, and against what.
  if (sender) {
    for (const recipient of recipients) {
      const dapp = dappForAddress(recipient.output.address);
      const order = readOrder(dapp?.name, recipient.output.datum, recipient.output);
      if (order) return describePendingSwap(sender.party, order, recipient.output, recipient.party);
    }
  }

  // What the tx says about itself beats anything inferred from its shape.
  if (tag) return describeTagged(tag, sender?.party, recipients, tx, sender?.wallet);

  // Nothing said, but spending a DEX's order script is itself a statement. Several
  // protocols settle without ever writing a message.
  const settlement = describeSettlement(tx, undefined, undefined, sender?.wallet);
  if (settlement) return settlement;

  // The funder only paid the fee and a protocol's own scripts did the moving — so the
  // protocol is the actor, not whoever relayed the transaction for it.
  if (sender) {
    const relayed = describeProtocolMove(tx, sender);
    if (relayed) return relayed;
  }

  // Nothing went to anyone else, but a script was spent: value came back out of a
  // contract rather than merely shuffling between the owner's own addresses.
  if (recipients.length === 0) {
    const unlock = describeUnlock(tx, sender?.party);
    if (unlock) return unlock;
  }

  if (!sender) return describeShared(tx, recipients);
  return describeTransfer(sender.party, recipients, tx.outputs, metadataLines(tx.metadata));
}

/**
 * Value coming back out of a contract, to the account that owns it.
 *
 * Nothing leaves the account in these, so there are no recipients and the sentence used
 * to fall through to "MOVED 1,992 ₳" — the account's own ADA, restated. What actually
 * happened is that a script was spent and released something: a vesting contract paying
 * out, collateral being reclaimed, a locked balance being drawn down. One real example
 * held 828.1 NIGHT, kept half and handed the other half to a payment address of the same
 * account, and read as a plain ADA shuffle.
 *
 * Measured across the script boundary rather than between addresses, since the addresses
 * are all the owner's: whatever the non-script side gained is what the script let go.
 */
function describeUnlock(tx: BlockTx, subject: Party | undefined): Intent | null {
  const fromScript = tx.inputs.filter((i) => !isWithdrawal(i) && i.address && paymentIsScript(i.address));
  if (fromScript.length === 0) return null;

  const held = (assets: AssetInfo[] | undefined, into: Map<string, { asset: AssetInfo; qty: ScaledQty }>) => {
    for (const asset of assets ?? []) {
      const seen = into.get(asset.fingerprint);
      const qty = parseQuantity(asset.quantity);
      into.set(asset.fingerprint, seen ? { asset, qty: addQuantities(seen.qty, qty) } : { asset, qty });
    }
  };
  const before = new Map<string, { asset: AssetInfo; qty: ScaledQty }>();
  const after = new Map<string, { asset: AssetInfo; qty: ScaledQty }>();
  let lovelaceBefore = 0n;
  let lovelaceAfter = 0n;
  for (const input of tx.inputs) {
    if (isWithdrawal(input) || !input.address || paymentIsScript(input.address)) continue;
    lovelaceBefore += BigInt(input.lovelace);
    held(input.assets, before);
  }
  for (const output of tx.outputs) {
    if (paymentIsScript(output.address)) continue;
    lovelaceAfter += BigInt(output.lovelace);
    held(output.assets, after);
  }

  const released: AssetInfo[] = [];
  for (const [fingerprint, { asset, qty }] of after) {
    const gained = subtractQuantities(qty, before.get(fingerprint)?.qty ?? [0n, 0]);
    if (gained[0] > 0n) released.push({ ...asset, quantity: formatScaled(gained) });
  }
  // ADA can only be counted as released once it exceeds the fee, which every tx spends
  // from this same side of the boundary.
  const releasedAda = lovelaceAfter + BigInt(tx.fee) - lovelaceBefore;
  if (released.length === 0 && releasedAda <= 0n) return null;

  const dapp = fromScript.map((i) => dappForAddress(i.address!)).find((d) => d !== undefined);
  return {
    subject,
    verb: 'UNLOCKED',
    amount: released.length > 0 ? undefined : { quantity: releasedAda.toString() },
    assets: released.length > 0 ? released : undefined,
    targets: [],
    hiddenTargets: 0,
    via: appParty(dapp?.name),
  };
}

/**
 * Several accounts funded this tx, so "who sent" has no single answer — it has several.
 *
 * They're named rather than counted. Payment addresses sharing a stake credential have
 * already been folded into one account by then, so what's left really is distinct
 * parties, and "3 WALLETS" hides the one thing a reader could act on.
 */
function describeShared(tx: BlockTx, recipients: Recipient[]): Intent {
  const funders = new Map<string, { lovelace: bigint; addresses: Set<string>; handle?: string }>();
  for (const input of tx.inputs) {
    if (isWithdrawal(input) || !input.address) continue;
    const account = walletOf(input.address);
    const seen = funders.get(account);
    if (!seen) {
      funders.set(account, {
        lovelace: BigInt(input.lovelace),
        addresses: new Set([input.address]),
        handle: input.handle,
      });
      continue;
    }
    seen.lovelace += BigInt(input.lovelace);
    seen.addresses.add(input.address);
    seen.handle ??= input.handle;
  }
  // Biggest contributor first: with only a few lines of room, that's the one most worth
  // showing if the rest have to be dropped. One address stays named by that address,
  // which is the more specific truth; several are named by the account they share.
  const sorted = [...funders.entries()].sort((a, b) => (b[1].lovelace > a[1].lovelace ? 1 : -1));
  const named = sorted.slice(0, MAX_SUBJECTS).map(([account, f]) => {
    const [only] = f.addresses;
    return partyForAddress(f.addresses.size === 1 ? only : account, f.handle);
  });
  const transfer = describeTransfer(undefined, recipients, tx.outputs, metadataLines(tx.metadata));
  return { ...transfer, subjects: named.length > 0 ? named : undefined };
}

/**
 * A protocol moving its own funds, relayed by somebody who only paid the fee.
 *
 * Lending and staking protocols reorganise their reserves constantly — splitting a
 * balance across UTXOs, rolling a market's state forward — and somebody has to submit
 * the transaction. That submitter puts in a UTXO and gets it back less the fee, so
 * naming them as the actor claims they did something they didn't: one real Liqwid tx
 * split 206,599 iUSD into four positions and read as the relayer having "ORDERED 3 ₳",
 * that being a min-UTXO on a script output.
 *
 * Null unless the funder really is a relayer — anyone whose own balance moved is a
 * participant, and the ordinary reading has more to say about them.
 */
function describeProtocolMove(tx: BlockTx, sender: Sender): Intent | null {
  const own = (address: string | null | undefined) => address != null && walletOf(address) === sender.wallet;
  let ownBefore = 0n;
  let ownAfter = 0n;
  for (const input of tx.inputs) {
    if (isWithdrawal(input) || !own(input.address)) continue;
    if (input.assets?.length) return null;
    ownBefore += BigInt(input.lovelace);
  }
  for (const output of tx.outputs) {
    if (!own(output.address)) continue;
    if (output.assets.length) return null;
    ownAfter += BigInt(output.lovelace);
  }
  // Everything they put in came back, less exactly the fee: they relayed, they didn't act.
  if (ownBefore === 0n || ownAfter + BigInt(tx.fee) !== ownBefore) return null;

  // Every script the tx spends has to belong to one dApp, or there's no single actor.
  const dapps = new Set<string>();
  const held = new Map<string, { asset: AssetInfo; qty: ScaledQty }>();
  for (const input of tx.inputs) {
    if (isWithdrawal(input) || !input.address || !paymentIsScript(input.address)) continue;
    const dapp = dappForAddress(input.address);
    if (!dapp) return null;
    dapps.add(dapp.name);
    for (const asset of input.assets ?? []) {
      const seen = held.get(asset.fingerprint);
      const qty = parseQuantity(asset.quantity);
      held.set(asset.fingerprint, seen ? { asset, qty: addQuantities(seen.qty, qty) } : { asset, qty });
    }
  }
  if (dapps.size !== 1) return null;

  // The largest holding it moved. Protocol scripts also carry identity NFTs, which say
  // nothing about the size of what happened.
  const moved = [...held.values()]
    .filter((h) => h.qty[0] > 1n)
    .sort((a, b) => Number(formatScaled(b.qty)) - Number(formatScaled(a.qty)))[0];
  if (!moved) return null;

  const quantity = formatScaled(moved.qty);
  return {
    subject: appParty([...dapps][0]),
    verb: 'MOVED',
    amount: {
      quantity,
      unit: assetLabel(moved.asset),
      fingerprint: moved.asset.fingerprint,
      image: { ...moved.asset, quantity },
    },
    targets: [],
    hiddenTargets: 0,
  };
}

/**
 * One asset is a headline, not a caption.
 *
 * A lone token carries the whole point of the transaction, and rendering its quantity as
 * a 9px label under a 96px thumbnail buried it: "UNLOCKED 1,549 NIGHT" read as a picture
 * with a footnote, while the same transaction denominated in ADA got the loud line. This
 * is the thumbnail rule one level down — one thing is shown big, several share the room.
 *
 * Several assets stay a group: there is no single figure to headline, and the quantities
 * belong with the art they each label. A lone NFT stays a picture: "1" is not a figure
 * anyone needs read to them.
 */
function headlineAsset(intent: Intent): Intent {
  if (intent.amount !== undefined || intent.assets?.length !== 1) return intent;
  const [asset] = intent.assets;
  // A single NFT has no figure worth reading: its art is its identity, and a loud "1"
  // above it would be noise where the picture is already the whole message.
  if (asset.quantity === '1') return intent;
  return {
    ...intent,
    amount: { quantity: asset.quantity, unit: assetLabel(asset), fingerprint: asset.fingerprint, image: asset },
    assets: undefined,
  };
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
