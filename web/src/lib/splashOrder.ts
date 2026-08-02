/**
 * Read a Splash Protocol swap order from its datum.
 *
 * Splash states both sides outright, which makes this the simplest of the order readers:
 * the asset going in, the exact amount of it, and the asset wanted back all sit at fixed
 * fields. No pool table is needed, unlike Minswap, where the pair has to be recovered
 * from an LP token.
 *
 * The field indices below were read off live mainnet orders and checked against the value
 * actually locked in each order UTXO — a 105 ₳ UTXO carrying a datum that says 100 ₳ in,
 * the remaining 5 ₳ being the protocol's fees and the deposit that comes back.
 */
import { asBytes, asInt, field, parseDatum, path } from './plutus';
import type { SwapOrder } from './dexOrder';

/** Field indices of Splash's order datum. */
const GIVE_ASSET = 2;
const GIVE_AMOUNT = 3;
const WANT_ASSET = 6;
/** Both assets are `(policy, name)` pairs; ADA is the empty pair. */
const POLICY = 0;
const NAME = 1;

export function readSplashOrder(datumHex: string | undefined): SwapOrder | null {
  const datum = parseDatum(datumHex);
  if (!datum) return null;

  const givePolicy = asBytes(path(datum, GIVE_ASSET, POLICY));
  const giveName = asBytes(path(datum, GIVE_ASSET, NAME));
  const wantPolicy = asBytes(path(datum, WANT_ASSET, POLICY));
  const wantName = asBytes(path(datum, WANT_ASSET, NAME));
  const giveAmount = asInt(field(datum, GIVE_AMOUNT));
  if (givePolicy === undefined || giveName === undefined) return null;
  if (wantPolicy === undefined || wantName === undefined) return null;
  if (giveAmount === undefined) return null;

  return {
    give: { policy: givePolicy, name: giveName },
    giveAmount,
    want: { policy: wantPolicy, name: wantName },
  };
}
