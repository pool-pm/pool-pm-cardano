/**
 * Read a Spectrum Finance swap order from its datum.
 *
 * The most self-describing of the three: it names both assets *and* carries both
 * amounts, so nothing has to be resolved and no direction flag has to be interpreted —
 * `base` is what's given and `quote` is what's wanted, whichever way round the trade is.
 *
 *     0 base            (policy, name)   what the user gives
 *     1 quote           (policy, name)   what they want
 *     2 poolNft         (policy, name)
 *     3 feeNum          int
 *     ...
 *     8 baseAmount      int              exactly what's going in
 *     9 minQuoteAmount  int              a slippage floor, so not reported
 *
 * Confirmed against real orders in both directions: `ADA → AGIX` at 22.004708 ₳ for
 * 75.98958249 AGIX (0.29 ₳ each, AGIX's actual price), and `LIFI → ADA`. Had base and
 * quote been the other way round, one of those two would have priced absurdly.
 */
import { asBytes, asInt, field, parseDatum, type PlutusData } from './plutus';
import type { OrderAsset, SwapOrder } from './dexOrder';

const BASE = 0;
const QUOTE = 1;
const BASE_AMOUNT = 8;

/** An asset as a `Constr(policy, name)` pair. */
function asset(data: PlutusData | undefined): OrderAsset | null {
  const policy = asBytes(field(data, 0));
  const name = asBytes(field(data, 1));
  return policy === undefined || name === undefined ? null : { policy, name };
}

/** The swap this order is asking for, or null when the datum isn't that shape. */
export function readSpectrumOrder(datumHex: string | undefined): SwapOrder | null {
  const datum = parseDatum(datumHex);
  if (!datum) return null;

  const give = asset(field(datum, BASE));
  const want = asset(field(datum, QUOTE));
  const giveAmount = asInt(field(datum, BASE_AMOUNT));
  if (!give || !want || giveAmount === undefined) return null;

  return { give, giveAmount, want };
}
