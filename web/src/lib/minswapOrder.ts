/**
 * Read a Minswap V2 swap order from its datum.
 *
 * An order is the first half of a swap: the user locks funds at Minswap's order script
 * saying what they want back, and a batcher settles it against the pool later. Until it
 * settles, the datum is the only place the intent exists — the transaction itself shows
 * value going to a script and nothing about what for.
 *
 * The datum's shape and every constant here come from Minswap's own SDK
 * (`src/types/order.ts`), not from inference. That matters most for the direction flag,
 * whose numbering is the reverse of the obvious guess:
 *
 *     Direction { B_TO_A = 0, A_TO_B = 1 }
 *
 * Getting that backwards would render every swap inverted while looking entirely
 * plausible, which is the same trap the pool table's hash check guards against.
 */
import { asBytes, asInt, constrTag, field, parseDatum, path } from './plutus';
import { isAda, poolPair, type PoolAsset } from './minswapPools';

/** Field indices of `OrderV2.Datum`, in the order the SDK declares them. */
const LP_ASSET = 5;
const STEP = 6;
/** `OrderV2.StepType.SWAP_EXACT_IN`. Other steps — deposits, withdrawals, routing — are
 *  different shapes and aren't a two-sided swap. */
const SWAP_EXACT_IN = 0;
/** Field indices of `OrderV2.SwapExactIn`. */
const DIRECTION = 0;
const SWAP_AMOUNT = 1;
const MINIMUM_RECEIVED = 2;
/** `OrderV2.Direction`. Note the numbering. */
const B_TO_A = 0;
/** `OrderV2.AmountType.SPECIFIC_AMOUNT` — the other variant swaps a computed balance,
 *  whose amount isn't known until settlement. */
const SPECIFIC_AMOUNT = 0;

export interface SwapOrder {
  /** What the user is giving. */
  give: PoolAsset;
  /** The exact amount of it, from the datum — not the order UTXO, which also holds the
   *  batcher fee and a deposit that come back. */
  giveAmount: bigint;
  /** What they want in return. */
  want: PoolAsset;
  /** The least they'll accept; the settled amount is this or better. */
  wantAtLeast: bigint;
}

/**
 * The swap this order is asking for, or null when it isn't one this can state exactly —
 * a step that isn't a plain swap, a swap of a balance whose size isn't yet known, or a
 * pool outside the bundled table.
 */
export function readSwapOrder(datumHex: string | undefined): SwapOrder | null {
  const datum = parseDatum(datumHex);
  if (!datum) return null;

  const step = field(datum, STEP);
  if (constrTag(step) !== SWAP_EXACT_IN) return null;

  const swapAmount = field(step, SWAP_AMOUNT);
  if (constrTag(swapAmount) !== SPECIFIC_AMOUNT) return null;
  const giveAmount = asInt(field(swapAmount, 0));
  const wantAtLeast = asInt(field(step, MINIMUM_RECEIVED));
  if (giveAmount === undefined || wantAtLeast === undefined) return null;

  const lpPolicy = asBytes(path(datum, LP_ASSET, 0));
  const lpName = asBytes(path(datum, LP_ASSET, 1));
  if (lpPolicy === undefined || lpName === undefined) return null;
  const pair = poolPair(lpPolicy, lpName);
  if (!pair) return null;

  const direction = constrTag(field(step, DIRECTION));
  if (direction === undefined) return null;
  const [a, b] = pair;
  const [give, want] = direction === B_TO_A ? [b, a] : [a, b];

  return { give, giveAmount, want, wantAtLeast };
}

export { isAda };
