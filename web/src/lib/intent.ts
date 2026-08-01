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
 * `describeTx` returns `null` for anything it can't state plainly — several senders, a
 * governance vote, an oracle update — and `Transaction.svelte` then falls back to the
 * raw input/output view. Adding a case here is always preferable to guessing: a
 * confident wrong sentence is worse than the raw view it replaces.
 */
import type { AssetInfo, BlockTx, DelegationInfo, MintInfo, TxInput, TxOutputInfo } from './types';
import { nonChangeOutputs } from './change';
import { stakeAddressOf } from './bech32';
import { dappForAddress, dappForPolicy, isDex, type Dapp } from './dapps';
import { formatTicker } from './layout';

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

/** A quantity in the sentence. `unit` absent means ADA and `quantity` is lovelace. */
export interface Amount {
  quantity: string;
  /** Token name for non-ADA amounts. */
  unit?: string;
  /** Asset fingerprint, so the amount can link to its asset page. */
  fingerprint?: string;
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
 */
function walletOf(address: string): string {
  return stakeAddressOf(address) ?? address;
}

interface Sender {
  party: Party;
  /** `walletOf` the funding address, for recognising its own outputs. */
  wallet: string;
}

/**
 * The single wallet that funded this tx, or null when more than one did. Among the
 * wallet's payment addresses, the one that contributed most is the one worth naming.
 */
function soleSender(inputs: TxInput[]): Sender | null {
  const wallets = new Map<string, TxInput>();
  for (const input of inputs) {
    if (isWithdrawal(input) || !input.address) continue;
    const wallet = walletOf(input.address);
    const seen = wallets.get(wallet);
    if (!seen || BigInt(input.lovelace) > BigInt(seen.lovelace)) wallets.set(wallet, input);
  }
  if (wallets.size !== 1) return null;
  const [wallet, input] = wallets.entries().next().value!;
  return { party: partyForAddress(input.address!, input.handle), wallet };
}

/** Sum the lovelace withdrawn from reward accounts by this tx. */
function withdrawn(inputs: TxInput[]): bigint {
  return inputs.filter(isWithdrawal).reduce((sum, i) => sum + BigInt(i.lovelace), 0n);
}

/** Merge outputs that go to the same address — one recipient, one line. */
function byRecipient(outputs: TxOutputInfo[]): TxOutputInfo[] {
  const merged = new Map<string, TxOutputInfo>();
  for (const output of outputs) {
    const seen = merged.get(output.address);
    if (!seen) {
      merged.set(output.address, { ...output, assets: [...output.assets] });
      continue;
    }
    seen.lovelace = (BigInt(seen.lovelace) + BigInt(output.lovelace)).toString();
    seen.assets.push(...output.assets);
  }
  return [...merged.values()];
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
      return isDex(dapp) ? 'SWAPPED' : 'ORDERED';
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

/** Build the target for one recipient output, headlining tokens over min-UTXO dust. */
function targetFor(output: TxOutputInfo): IntentTarget {
  const party = partyForAddress(output.address, output.handle);
  const lovelace = BigInt(output.lovelace);
  const carriesTokens = output.assets.length > 0;
  return {
    party,
    assets: carriesTokens ? output.assets : undefined,
    amount: carriesTokens && lovelace <= MIN_UTXO_DUST ? undefined : { quantity: output.lovelace },
  };
}

/**
 * The default reading: value leaving one wallet for others. A single recipient gets the
 * full sentence with the amount on its own loud line; several recipients keep their
 * amounts next to their names, since there's no one number to headline.
 */
function describeTransfer(subject: Party, recipients: TxOutputInfo[], outputs: TxOutputInfo[]): Intent {
  if (recipients.length === 0) {
    // Everything came back to the sender: a wallet reorganising its own UTXOs.
    const moved = outputs.reduce((sum, o) => sum + BigInt(o.lovelace), 0n);
    return { subject, verb: 'MOVED', amount: { quantity: moved.toString() }, targets: [], hiddenTargets: 0 };
  }

  if (recipients.length === 1) {
    const [output] = recipients;
    const target = targetFor(output);
    const dapp = dappForAddress(output.address);
    const verb = dapp ? verbForDapp(dapp) : null;
    if (verb) {
      // The dApp is the venue, not the recipient: `$bob SWAPPED 100 ₳ ON MINSWAP`.
      return {
        subject,
        verb,
        amount: target.amount,
        assets: target.assets,
        targets: [],
        hiddenTargets: 0,
        via: target.party,
      };
    }
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

  const sorted = [...recipients].sort((a, b) => (BigInt(b.lovelace) > BigInt(a.lovelace) ? 1 : -1));
  return {
    subject,
    verb: 'SENT',
    preposition: 'TO',
    targets: sorted.slice(0, MAX_TARGETS).map(targetFor),
    hiddenTargets: Math.max(0, sorted.length - MAX_TARGETS),
  };
}

/**
 * `$bob MINTED <thumbnails> ON JPG.STORE`, or `$bob BURNED 3 TOKENS`.
 *
 * A mint is otherwise invisible: the new token sits in an output looking exactly like
 * one that was transferred, and since it usually lands back in the minter's own wallet
 * the tx would read as `MOVED`. A burn is worse — nothing in the outputs records it.
 */
function describeMint(tx: BlockTx, mint: MintInfo, subject: Party | undefined, recipients: TxOutputInfo[]): Intent {
  const app = mint.policies.map(dappForPolicy).find((d) => d !== undefined);
  const via: Party | undefined = app ? { label: app.name.toUpperCase(), kind: 'app' } : undefined;

  if (mint.minted === 0) {
    // Nothing to show a thumbnail of — the assets are gone.
    return {
      subject,
      verb: 'BURNED',
      amount: { quantity: String(mint.burned), unit: mint.burned === 1 ? 'TOKEN' : 'TOKENS' },
      targets: [],
      hiddenTargets: 0,
      via,
    };
  }

  const created = new Set(mint.fingerprints ?? []);
  // The minted assets are in *some* output — usually the minter's own, which the
  // recipient filter has already dropped — so look across all of them.
  const assets = tx.outputs.flatMap((o) => o.assets.filter((a) => created.has(a.fingerprint)));
  // Minting straight to someone else is worth saying; minting to yourself isn't.
  const target = recipients.length === 1 ? partyForAddress(recipients[0].address, recipients[0].handle) : undefined;
  return {
    subject,
    verb: 'MINTED',
    amount: assets.length > 0 ? undefined : { quantity: String(mint.minted), unit: 'TOKENS' },
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

  if (mint) return describeMint(tx, mint, sender?.party, recipients);

  const rewards = withdrawn(tx.inputs);
  if (rewards > 0n && recipients.length === 0) return describeWithdrawal(tx, rewards, sender?.party);

  if (!sender) return null; // several wallets funded it — "who sent" has no answer
  return describeTransfer(sender.party, recipients, tx.outputs);
}

/**
 * The recipients that are somebody else. `nonChangeOutputs` reads an output exceeding
 * what its own address put in as a receipt, which is right for a feed but not for a
 * sentence: value landing on another payment address of the sender's own account hasn't
 * been sent anywhere.
 */
function outsideRecipients(tx: BlockTx, wallet?: string): TxOutputInfo[] {
  const merged = byRecipient(nonChangeOutputs(tx.inputs, tx.outputs));
  return wallet === undefined ? merged : merged.filter((o) => walletOf(o.address) !== wallet);
}
