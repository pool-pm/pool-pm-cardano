/**
 * A pending swap, however the DEX that took it happens to write its datum.
 *
 * Each protocol's order datum is its own shape, and they disagree about what they even
 * record: SundaeSwap names both assets outright, while Minswap names only the pool via
 * its LP token and leaves the pair to be resolved. What they have in common is the one
 * thing a reader wants — what's going in, and what's wanted back — so that's the shape
 * they're read into.
 *
 * Dispatch is by the dApp owning the script the order sits at, never by trying each
 * decoder in turn: two datums of different protocols can be structurally similar enough
 * that a speculative parse succeeds and reports a swap that was never asked for.
 */
import { readSwapOrder as readMinswapOrder } from './minswapOrder';
import { readSundaeSwapOrder } from './sundaeOrder';
import { readSpectrumOrder } from './spectrumOrder';
import { readSplashOrder } from './splashOrder';
import { readCSwapOrder } from './cswapOrder';

/** One side of a swap: the asset's on-chain identity. ADA is `("", "")`. */
export interface OrderAsset {
  policy: string;
  name: string;
}

export interface SwapOrder {
  /** What the user is giving. */
  give: OrderAsset;
  /** The exact amount of it, from the datum — not the order UTXO, which also holds the
   *  protocol's fee and a deposit that come back. Absent where the protocol doesn't
   *  record it (CSwap states only the output it wants), leaving the UTXO as the only
   *  source and its ADA side slightly overstated by the fee inside it. */
  giveAmount?: bigint;
  /**
   * What they want in return, named but not counted. Every one of these datums records
   * only a minimum — a slippage floor rather than a forecast — and the fill is nearly
   * always better, so the amount would claim a precision the order doesn't have. The
   * settlement says what actually arrived.
   */
  want: OrderAsset;
}

/** ADA has no policy or name to look up. */
export function isAda(asset: OrderAsset): boolean {
  return asset.policy === '' && asset.name === '';
}

/** The UTXO an order was posted in, for the protocols whose datum doesn't state what's
 *  going in — only what's wanted back. */
export interface OrderUtxo {
  lovelace: string;
  assets?: { name?: string; quantity: string }[];
}

/** The swap `datumHex` asks for, if `dapp` is a DEX whose orders we can read. */
export function readOrder(dapp: string | undefined, datumHex: string | undefined, utxo?: OrderUtxo): SwapOrder | null {
  switch (dapp) {
    case 'Minswap':
      return readMinswapOrder(datumHex);
    case 'SundaeSwap':
      return readSundaeSwapOrder(datumHex);
    case 'Spectrum Finance':
      return readSpectrumOrder(datumHex);
    case 'Splash Protocol':
      return readSplashOrder(datumHex);
    case 'CSWAP DEX':
      return readCSwapOrder(datumHex, utxo);
    default:
      return null;
  }
}
