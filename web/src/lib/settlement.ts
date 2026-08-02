/**
 * Read a settled DEX order from the pool's own balance change.
 *
 * A swap on an AMM is two transactions. The user posts an order — funds locked at the
 * protocol's order script, with a datum stating what they want back. Later the
 * protocol's batcher spends that order together with the pool UTXO, computes the result
 * against the pool's reserves, and pays the user out.
 *
 * The settlement is the tx where the whole swap becomes visible without decoding
 * anything, because the pool is re-created with its new balances:
 *
 *     IN   pool     2,109,561 ₳ + 19,858,645,356,383 NIGHT
 *     IN   batcher    275.6 ₳                    ← also an output; that's how it's spotted
 *     IN   order        4.0 ₳ +     42,126,614,528 NIGHT
 *     OUT  batcher    276.9 ₳
 *     OUT  user     4,454.2 ₳                    ← absent from the inputs
 *     OUT  pool     2,105,109 ₳ + 19,900,771,970,911 NIGHT
 *
 * What the pool gained is what the user gave; what it lost is what the user got. Both
 * exact. Reading the order UTXO instead would overstate the ADA side, since it also
 * holds the batcher fee and a deposit that come back — which is the trap this avoids.
 */
import type { AssetInfo, TxInput, TxOutputInfo } from './types';
import { parseQuantity, type ScaledQty } from './change';
import { dappForAddress } from './dapps';

/**
 * The index the server gives a withdrawal's pseudo-input. It carries a reward address,
 * not a UTXO — and a reward address matches a dApp whenever it happens to share the
 * dApp's stake credential, so counting one as an order UTXO reads a plain withdrawal as
 * a settled swap.
 */
const WITHDRAWAL_INDEX = -1;

/** One side of a swap: ADA when `asset` is absent. */
export interface Side {
  /** Lovelace, or the token's decimals-formatted quantity. */
  quantity: string;
  asset?: AssetInfo;
}

export interface Settlement {
  /** What the user handed over — the pool's gain. */
  gave: Side;
  /** What the user received — the pool's loss. */
  got: Side;
  /** The address paid out, for naming the user. */
  beneficiary: TxOutputInfo;
}

function isPool(address: string | null | undefined): boolean {
  return address !== null && address !== undefined && dappForAddress(address)?.role === 'pool';
}

/**
 * The assets that exist anywhere outside the pool in this transaction.
 *
 * A pool holds more than the pair it trades. Minswap V2 keeps a pool NFT and an LP token
 * whose remaining supply tracks liquidity, and that supply figure moves when a swap
 * happens — so the pool showed three assets changing where a swap has two, and the whole
 * reading was abandoned as "not this shape". Neither of those ever leaves the pool, which
 * is exactly what separates them from the two sides: the asset going in arrives from the
 * order UTXO, and the asset coming out lands in the payout.
 */
function assetsOutsidePool(inputs: TxInput[], outputs: TxOutputInfo[]): Set<string> {
  const outside = new Set<string>();
  for (const input of inputs) {
    if (isPool(input.address)) continue;
    for (const asset of input.assets ?? []) outside.add(asset.fingerprint);
  }
  for (const output of outputs) {
    if (isPool(output.address)) continue;
    for (const asset of output.assets) outside.add(asset.fingerprint);
  }
  return outside;
}

/** The pool UTXO on each side of the tx, when exactly one pool was touched. */
function poolPair(inputs: TxInput[], outputs: TxOutputInfo[]): [TxInput, TxOutputInfo] | null {
  const ins = inputs.filter((i) => isPool(i.address));
  const outs = outputs.filter((o) => isPool(o.address));
  // More than one pool is a routed swap through several pairs — its ends don't line up
  // with a single gain and loss, so it isn't this shape.
  if (ins.length !== 1 || outs.length !== 1 || ins[0].address !== outs[0].address) return null;
  return [ins[0], outs[0]];
}

/** Assets keyed by fingerprint, for diffing two sides of the same pool. */
function byFingerprint(assets: AssetInfo[] | undefined): Map<string, AssetInfo> {
  return new Map((assets ?? []).map((a) => [a.fingerprint, a]));
}

/** `a - b`, aligning decimals. */
function difference(a: ScaledQty, b: ScaledQty): ScaledQty {
  const scale = Math.max(a[1], b[1]);
  return [a[0] * 10n ** BigInt(scale - a[1]) - b[0] * 10n ** BigInt(scale - b[1]), scale];
}

/** Render a scaled quantity back to the decimal string the rest of the code expects. */
function formatScaled([value, decimals]: ScaledQty): string {
  const negative = value < 0n;
  const digits = (negative ? -value : value).toString().padStart(decimals + 1, '0');
  const whole = digits.slice(0, digits.length - decimals);
  const fraction = decimals === 0 ? '' : digits.slice(digits.length - decimals).replace(/0+$/, '');
  return (negative ? '-' : '') + whole + (fraction ? '.' + fraction : '');
}

/**
 * Every asset whose pool balance moved, as `(fingerprint, signed change)`. A swap moves
 * exactly two things in opposite directions — one of which is usually ADA.
 */
function poolDeltas(pool: TxInput, next: TxOutputInfo): { side: Side; change: bigint }[] {
  const moved: { side: Side; change: bigint }[] = [];

  const lovelace = BigInt(next.lovelace) - BigInt(pool.lovelace);
  if (lovelace !== 0n) {
    const magnitude = lovelace < 0n ? -lovelace : lovelace;
    moved.push({ side: { quantity: magnitude.toString() }, change: lovelace });
  }

  const before = byFingerprint(pool.assets);
  const after = byFingerprint(next.assets);
  for (const fingerprint of new Set([...before.keys(), ...after.keys()])) {
    const asset = after.get(fingerprint) ?? before.get(fingerprint)!;
    const delta = difference(
      parseQuantity(after.get(fingerprint)?.quantity ?? '0'),
      parseQuantity(before.get(fingerprint)?.quantity ?? '0'),
    );
    if (delta[0] === 0n) continue;
    const magnitude: ScaledQty = [delta[0] < 0n ? -delta[0] : delta[0], delta[1]];
    moved.push({
      side: { quantity: formatScaled(magnitude), asset: { ...asset, quantity: formatScaled(magnitude) } },
      change: delta[0],
    });
  }
  return moved;
}

/**
 * The account an output belongs to, for telling the user's payout from everyone else's.
 * Kept local rather than shared so this module doesn't depend on the sentence builder.
 */
type AccountOf = (address: string) => string;

/**
 * The one output that is somebody being paid: an address that put nothing into this tx.
 *
 * The batcher funds the settlement and takes its change back, and the pool is re-created
 * where it was — so both appear on the input side. The user does not, having posted their
 * order in an earlier transaction. That single asymmetry identifies them without needing
 * to know who any batcher is.
 */
function beneficiary(inputs: TxInput[], outputs: TxOutputInfo[], accountOf: AccountOf): TxOutputInfo | null {
  const funders = new Set(inputs.filter((i) => i.address).map((i) => accountOf(i.address!)));
  const paid = outputs.filter((o) => !funders.has(accountOf(o.address)) && !dappForAddress(o.address));
  return paid.length === 1 ? paid[0] : null;
}

/**
 * The swap this settlement performed, or null when it isn't the single-order shape this
 * can state exactly — several orders share one pool movement, and splitting an aggregate
 * between them would be a guess.
 */
export function readSettlement(inputs: TxInput[], outputs: TxOutputInfo[], accountOf: AccountOf): Settlement | null {
  const orders = inputs.filter(
    (i) => i.index !== WITHDRAWAL_INDEX && i.address && dappForAddress(i.address)?.role === 'order',
  );
  if (orders.length !== 1) return null;

  const pool = poolPair(inputs, outputs);
  if (!pool) return null;

  // Only assets that exist outside the pool can be a side of the swap; the pool's own
  // bookkeeping tokens change without anybody trading them.
  const outside = assetsOutsidePool(inputs, outputs);
  const moved = poolDeltas(pool[0], pool[1]).filter((m) => !m.side.asset || outside.has(m.side.asset.fingerprint));
  const gained = moved.filter((m) => m.change > 0n);
  const lost = moved.filter((m) => m.change < 0n);
  // A swap is one thing in and one thing out. Anything else — a deposit, a withdrawal,
  // a pool whose fee token also moved — isn't a shape this sentence describes.
  if (gained.length !== 1 || lost.length !== 1) return null;

  const paid = beneficiary(inputs, outputs, accountOf);
  if (!paid) return null;

  return { gave: gained[0].side, got: lost[0].side, beneficiary: paid };
}
