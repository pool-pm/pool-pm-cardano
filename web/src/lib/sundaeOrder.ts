/**
 * Read a SundaeSwap V3 swap order from its datum.
 *
 * Unlike Minswap's, this datum names both assets outright — it carries
 * `(policy, name, amount)` for each side — so it needs no pool table to resolve. The
 * shape and every constructor index below come from SundaeSwap's own SDK
 * (`Contract.v3.ts`), not from inference:
 *
 *     OrderDatum (ctor 0): [poolIdent, owner, maxProtocolFee, destination, details, extension]
 *     Order union:  0 Strategy   1 Swap{offer, minReceived}   2 Deposit
 *                   3 Withdrawal 4 Donation                   5 Record
 */
import { asBytes, asInt, constrTag, field, parseDatum, type PlutusData } from './plutus';
import type { SwapOrder } from './dexOrder';

/** Field index of `details` in `OrderDatum`. */
const DETAILS = 4;
/** `Order.Swap` — the only variant that's a two-sided trade. */
const SWAP = 1;
/** Field indices within `Swap`. */
const OFFER = 0;
const MIN_RECEIVED = 1;

/** `Tuple$ByteArray_ByteArray_Int` — a CBOR list of policy, name, amount. */
function tuple(data: PlutusData | undefined): { policy: string; name: string; amount: bigint } | null {
  if (data?.kind !== 'list' || data.items.length !== 3) return null;
  const policy = asBytes(data.items[0]);
  const name = asBytes(data.items[1]);
  const amount = asInt(data.items[2]);
  if (policy === undefined || name === undefined || amount === undefined) return null;
  return { policy, name, amount };
}

/**
 * The swap this order is asking for, or null when it isn't one — a strategy, a deposit,
 * a withdrawal, or a shape this doesn't recognise.
 */
export function readSundaeSwapOrder(datumHex: string | undefined): SwapOrder | null {
  const details = field(parseDatum(datumHex), DETAILS);
  if (constrTag(details) !== SWAP) return null;

  const offer = tuple(field(details, OFFER));
  const wanted = tuple(field(details, MIN_RECEIVED));
  if (!offer || !wanted) return null;

  return {
    give: { policy: offer.policy, name: offer.name },
    giveAmount: offer.amount,
    want: { policy: wanted.policy, name: wanted.name },
  };
}
