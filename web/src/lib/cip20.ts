/**
 * Read what a transaction says about itself.
 *
 * By convention most Cardano dApps write a CIP-20 message (metadata label 674) naming
 * themselves and the action, because wallets show it — `"Minswap: Market Order"`,
 * `"WingRiders: V2 Swap"`, `"Surf - Borrow - ADA / NIGHT"`. That is the protocol's own
 * statement about the tx, and it needs no address registry, no datum decoding and no
 * per-protocol code, so it outranks every other signal we have. Over a 30k-block window
 * ~19% of mainnet txs carry a message and the dApp-written ones dominate it.
 *
 * The same field also carries genuine human memos — "Test", "1234", "MLB | Yankees win
 * vs Phillies" — so a line only reads as a protocol tag when it *starts* with a name we
 * already know. Inventing an app from arbitrary text would put a confident wrong
 * sentence on the tile, which is worse than the message line we render today.
 */
import { registryAppNames } from './dapps';

export interface TaggedAction {
  /** The dApp, in the casing it wrote (`"Minswap"`, `"Surf"`). */
  app: string;
  /** What it said it was doing, verbatim after the name (`"Market Order"`). */
  action?: string;
  /** `action` mapped to a sentence verb, when it maps to one. */
  verb?: string;
}

/**
 * dApps observed naming themselves on mainnet that the CRFA registry doesn't list.
 * Every entry here is a name seen in a real tx message, not a guess — the registry is
 * the primary vocabulary and this is the tail it hasn't caught up with.
 */
const EXTRA_APPS = [
  'SteelSwap',
  'Surf',
  'Bodega Market',
  'Dano Finance',
  'Danogo',
  'Ourodex',
  'CNFT TOOLS',
  'VESPR',
  'GeniusYield',
  'VyFi',
  'Masumi',
  'Viperion',
  'Toolheads',
];

/**
 * Registry names carry a qualifier the on-chain message usually drops ("Splash
 * Protocol" writes "Splash"), so each name is also matched without its trailing
 * business word.
 */
const QUALIFIERS = /\s+(Protocol|Finance|Platform|Labs|Network|Bond|Tokens|DEX|Marketplace)$/i;

/** Prefixes a dApp puts before its own name; stripped before matching. */
const TOOL_PREFIX = /^(SDK|App|Web|Mobile)\s+/i;

/** What separates the name from the action: `Minswap: …`, `Surf - …`, `Dexhunter Trade`. */
const SEPARATOR = /^[\s:|\-–—+·]+/;

function normalize(s: string): string {
  return s.toLowerCase().replace(/[^a-z0-9]/g, '');
}

/** Longest name first, so "Bodega Market" wins over a hypothetical "Bodega". */
const VOCABULARY: { key: string; name: string }[] = [
  ...new Set([...registryAppNames(), ...registryAppNames().map((n) => n.replace(QUALIFIERS, '')), ...EXTRA_APPS]),
]
  .map((name) => ({ key: normalize(name), name }))
  .filter((e) => e.key.length >= 3) // "Axo" is the shortest real one; below that it's noise
  .sort((a, b) => b.key.length - a.key.length);

/**
 * `action` → the verb it means. First match wins, so the order is specificity order:
 * "Cancel Order" is a cancellation before it's an order, "Order Executed" is an
 * execution before it's an order, "Stake liquidity" is a deposit before it's staking.
 */
const VERBS: [RegExp, string][] = [
  [/cancel/i, 'CANCELLED'],
  [/refund/i, 'REFUNDED'],
  [/distribut|payout/i, 'DISTRIBUTED'],
  [/execut|batch|match|process/i, 'EXECUTED'],
  [/liquidat/i, 'LIQUIDATED'],
  [/claim|airdrop|harvest|reward/i, 'CLAIMED'],
  [/deposit|supply|add liquidity|stake liquidity|add collateral|provide/i, 'DEPOSITED'],
  [/withdraw|remove liquidity|redeem|unstake/i, 'WITHDREW'],
  [/repay/i, 'REPAID'],
  [/borrow|leverage/i, 'BORROWED'],
  [/zap/i, 'ZAPPED'],
  [/swap|trade|market order|routing/i, 'SWAPPED'],
  [/buy|bid|purchase/i, 'BOUGHT'],
  [/sell|listing/i, 'SOLD'],
  [/mint/i, 'MINTED'],
  [/burn/i, 'BURNED'],
  [/stak|delegat/i, 'STAKED'],
  [/vote|poll/i, 'VOTED'],
  [/order|request/i, 'ORDERED'],
];

/** The verb `action` means, or undefined when it names something we have no word for. */
export function verbForAction(action: string): string | undefined {
  for (const [pattern, verb] of VERBS) if (pattern.test(action)) return verb;
  return undefined;
}

/** A dApp named mid-line rather than at the start: "Cancellation via MuesliSwap". */
const VIA = /\bvia\s+(.+)$/i;

/**
 * The dApp and action stated by a line that *starts* with the dApp's name, or null.
 * Anchoring at the start is what keeps a human memo ("Get Rugged! -HOSKY") from being
 * read as a protocol naming itself.
 */
function parsePrefixed(line: string): TaggedAction | null {
  const text = line.trim().replace(TOOL_PREFIX, '');
  const key = normalize(text);
  if (!key) return null;
  const match = VOCABULARY.find((entry) => key.startsWith(entry.key));
  if (!match) return null;

  // Walk the raw text past however the name was spelled, then past the separator.
  let consumed = 0;
  let matched = 0;
  while (consumed < text.length && matched < match.key.length) {
    if (/[a-z0-9]/i.test(text[consumed])) matched++;
    consumed++;
  }
  const action = text.slice(consumed).replace(SEPARATOR, '').trim();
  // A bare version ("SteelSwap: 1.18.0") names the app but no action.
  if (!action || /^[\d.\s]+$/.test(action)) return { app: match.name };
  return { app: match.name, action, verb: verbForAction(action) };
}

/**
 * The dApp and action a message line states, or null if the line isn't one — a memo, a
 * partner tag, a bare version string.
 *
 * Beyond the plain `Name: Action` form, two shapes put the name elsewhere: after "via"
 * ("Cancellation via MuesliSwap Aggregator"), and after the separator, where the action
 * comes first ("Cardano Batcher Order: MINSWAP_V2"). Both are re-read from the part
 * that holds the name, keeping the rest as the action.
 */
function parseLine(line: string): TaggedAction | null {
  const direct = parsePrefixed(line);
  if (direct) return direct;

  const via = VIA.exec(line);
  if (via) {
    const tag = parsePrefixed(via[1]);
    if (tag) {
      const action = line.slice(0, via.index).trim();
      return action ? { app: tag.app, action, verb: verbForAction(action) } : tag;
    }
  }

  const separator = line.search(/[:|–—]|\s-\s/);
  if (separator > 0) {
    const tag = parsePrefixed(line.slice(separator).replace(SEPARATOR, ''));
    if (tag) {
      const action = line.slice(0, separator).trim();
      return action ? { app: tag.app, action, verb: verbForAction(action) } : tag;
    }
  }
  return null;
}

/**
 * The strongest tag across a tx's message lines: the first line naming a dApp *and* an
 * action it maps to a verb, else the first naming a dApp at all. A tx often carries
 * several lines — the dApp's own, then partner and aggregator tags — and the one that
 * says what happened is the one worth reading.
 */
export function parseMessage(lines: string[] | undefined): TaggedAction | null {
  if (!lines?.length) return null;
  let tagged: TaggedAction | null = null;
  let fallback: TaggedAction | null = null;
  for (const line of lines) {
    const tag = parseLine(line);
    if (!tag) continue;
    if (tag.verb) {
      tagged = tag;
      break;
    }
    fallback ??= tag;
  }
  tagged ??= fallback;
  if (!tagged) return null;

  // A cancellation is the one case where the dApp's own line describes the order being
  // cancelled rather than what this tx does — DexHunter tags a cancel with the original
  // "Dexhunter Trade" plus a bare "cancel" line. A standalone word overrides.
  if (lines.some((l) => /^\s*cancel(l?ed|lation)?\s*$/i.test(l))) {
    return { ...tagged, verb: 'CANCELLED' };
  }
  return tagged;
}
