/**
 * Read a CSwap DEX swap order from its datum.
 *
 * CSwap records the *output* it wants, not the input it's giving: the datum's second
 * field is the exact bundle the order must be paid — the wanted token plus the minimum
 * ADA any UTXO has to carry. What's being given is simply what the order UTXO holds, so
 * that side is read from the UTXO rather than the datum.
 *
 * Read off live mainnet orders and checked against each order UTXO: one holding 1,002.69 ₳
 * and no tokens against a datum asking for 9,803.921568 NIGHT, another holding 186.29 ₳
 * against a datum asking for 102,630 SNEK.
 */
import { asBytes, asList, field, item, parseDatum } from './plutus';
import type { OrderAsset, OrderUtxo, SwapOrder } from './dexOrder';

/** Field index of the bundle the order is to be paid. */
const WANTED = 1;
/** Each entry of that bundle is `[policy, name, amount]`. */
const POLICY = 0;
const NAME = 1;

const ADA: OrderAsset = { policy: '', name: '' };
/**
 * A give side the datum never names, to be read off the order UTXO instead — which holds
 * exactly the token being swapped, already scaled and named by the server. Deliberately
 * not the empty pair, which means ADA.
 */
const HELD_IN_UTXO: OrderAsset = { policy: '?', name: '' };

export function readCSwapOrder(datumHex: string | undefined, utxo: OrderUtxo | undefined): SwapOrder | null {
  const datum = parseDatum(datumHex);
  if (!datum || !utxo) return null;

  const wanted = asList(field(datum, WANTED));
  if (wanted.length === 0) return null;
  // The bundle always includes a bare-ADA entry for the min-UTXO the payout has to carry.
  // That's an artefact of how Cardano outputs work, not what the order is for, so the
  // token entry wins whenever there is one.
  const assets: OrderAsset[] = [];
  for (const entry of wanted) {
    const policy = asBytes(item(entry, POLICY));
    const name = asBytes(item(entry, NAME));
    if (policy === undefined || name === undefined) return null;
    assets.push({ policy, name });
  }
  const want = assets.find((a) => a.policy !== '') ?? assets[0];

  // A token order's UTXO holds exactly the token being swapped; an ADA order's holds the
  // ADA plus the protocol's fee, which the datum doesn't break out — so an ADA amount
  // here is what was locked, slightly more than what will be traded.
  const held = utxo.assets ?? [];
  const give: OrderAsset = held.length === 1 ? HELD_IN_UTXO : ADA;
  const giveAmount = give === ADA ? BigInt(utxo.lovelace) : undefined;

  // Giving and wanting ADA both isn't a swap — a malformed or unusual order.
  if (give === ADA && want.policy === '') return null;

  return { give, giveAmount, want };
}
