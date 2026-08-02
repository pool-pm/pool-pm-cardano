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
import { asBytes, asInt, asList, constrTag, field, parseDatum, path, type PlutusData } from './plutus';
import { poolPair } from './minswapPools';
import type { OrderAsset, SwapOrder } from './dexOrder';

/** Field indices of `OrderV2.Datum`, in the order the SDK declares them. */
const LP_ASSET = 5;
const STEP = 6;
/** `OrderV2.StepType.SWAP_EXACT_IN`, the plain one-pool swap. */
const SWAP_EXACT_IN = 0;
/** `OrderV2.StepType.SWAP_MULTI_ROUTING`: one order routed through several pools, to
 *  reach a pair that has no pool of its own. Common enough that leaving it out was the
 *  main reason a Minswap batch still read as "EXECUTED 1 ORDER". */
const SWAP_MULTI_ROUTING = 9;
/** Field indices of `OrderV2.SwapExactIn`. */
const DIRECTION = 0;
const SWAP_AMOUNT = 1;
const MINIMUM_RECEIVED = 2;
/** Field indices of `OrderV2.SwapMultiRouting`, which leads with the route list. */
const ROUTINGS = 0;
const ROUTING_SWAP_AMOUNT = 1;
const ROUTING_MINIMUM_RECEIVED = 2;
/** Field indices of one `OrderV2.Route`. */
const ROUTE_LP_ASSET = 0;
const ROUTE_DIRECTION = 1;
/** `OrderV2.Direction`. Note the numbering. */
const B_TO_A = 0;
/** `OrderV2.AmountType.SPECIFIC_AMOUNT` — the other variant swaps a computed balance,
 *  whose amount isn't known until settlement. */
const SPECIFIC_AMOUNT = 0;

/** The two ends of one hop: what goes into that pool and what comes out of it. */
function hop(lpAsset: PlutusData | undefined, direction: PlutusData | undefined): [OrderAsset, OrderAsset] | null {
  const lpPolicy = asBytes(path(lpAsset, 0));
  const lpName = asBytes(path(lpAsset, 1));
  if (lpPolicy === undefined || lpName === undefined) return null;
  const pair = poolPair(lpPolicy, lpName);
  if (!pair) return null;

  const tag = constrTag(direction);
  if (tag === undefined) return null;
  const [a, b] = pair;
  return tag === B_TO_A ? [b, a] : [a, b];
}

/**
 * The exact amount going in, when the step states one.
 *
 * The other `AmountType` swaps a computed balance whose size isn't known until
 * settlement, so there is no figure to report for it.
 */
function specificAmount(step: PlutusData | undefined, index: number): bigint | undefined {
  const swapAmount = field(step, index);
  if (constrTag(swapAmount) !== SPECIFIC_AMOUNT) return undefined;
  return asInt(field(swapAmount, 0));
}

/**
 * The swap this order is asking for, or null when it isn't one this can state exactly —
 * a step that isn't a swap, a swap of a balance whose size isn't yet known, or a pool
 * outside the bundled table.
 */
export function readSwapOrder(datumHex: string | undefined): SwapOrder | null {
  const datum = parseDatum(datumHex);
  if (!datum) return null;

  const step = field(datum, STEP);
  switch (constrTag(step)) {
    case SWAP_EXACT_IN: {
      const giveAmount = specificAmount(step, SWAP_AMOUNT);
      // The minimum is read only to confirm this is a well-formed swap step; it isn't
      // reported, being a slippage floor rather than what will actually arrive.
      const minimum = asInt(field(step, MINIMUM_RECEIVED));
      if (giveAmount === undefined || minimum === undefined) return null;

      const ends = hop(field(datum, LP_ASSET), field(step, DIRECTION));
      if (!ends) return null;
      return { give: ends[0], giveAmount, want: ends[1] };
    }
    case SWAP_MULTI_ROUTING: {
      // One order crossing several pools to reach a pair that has none of its own. Only
      // the ends of the chain are the swap: what the first hop takes in and what the last
      // hands back. The intermediate asset is a mechanism, not something the user asked
      // for — this order gave USDM and wanted STRIKE, hopping through ADA to do it.
      const routes = asList(field(step, ROUTINGS));
      if (routes.length === 0) return null;
      const giveAmount = specificAmount(step, ROUTING_SWAP_AMOUNT);
      const minimum = asInt(field(step, ROUTING_MINIMUM_RECEIVED));
      if (giveAmount === undefined || minimum === undefined) return null;

      const first = hop(field(routes[0], ROUTE_LP_ASSET), field(routes[0], ROUTE_DIRECTION));
      const last = hop(
        field(routes[routes.length - 1], ROUTE_LP_ASSET),
        field(routes[routes.length - 1], ROUTE_DIRECTION),
      );
      // Every pool on the route has to be known: an unresolved hop at either end leaves
      // the swap's own ends unnamed.
      if (!first || !last) return null;
      return { give: first[0], giveAmount, want: last[1] };
    }
    default:
      return null;
  }
}
