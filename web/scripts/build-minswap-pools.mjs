// Regenerate `src/lib/minswap-pools.json` from Minswap's public pool metrics.
//
//   node scripts/build-minswap-pools.mjs
//
// A Minswap order datum names the pool it targets by its LP token, never by the pair:
//
//   lp_asset.policy = f5808c2c…          one shared policy for every V2 pool
//   lp_asset.name   = sha3_256( sha3_256(policyA ++ nameA) ++ sha3_256(policyB ++ nameB) )
//
// That's a digest, so it can't be inverted — which is why a table is needed at all to
// turn a pending swap into "100 ₳ for 250 MIN" rather than "100 ₳ for something".
//
// But it *can* be recomputed, so every entry is verified here before it ships: a pair
// that doesn't hash back to its LP name is dropped. That matters more than it sounds,
// because the A/B order is load-bearing — the order datum's `direction` field selects
// between them, so a reversed pair would render every swap on that pool backwards while
// still looking entirely plausible. The hash catches exactly that.

import { createHash } from 'node:crypto';
import { writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const API = 'https://api-mainnet-prod.minswap.org/v1/pools/metrics';
const OUT = join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'lib', 'minswap-pools.json');
/** The API caps a page at 100. */
const PAGE = 100;
/** Verified V2 pools to keep, by liquidity. The tail is pools nobody swaps through. */
const POOLS = 500;
/** Minswap V2's LP policy — one policy, every V2 pool (their SDK's `lpPolicyId`).
 *  V1 and the stableswap pools mint under different policies and derive their LP names
 *  differently, and their orders use a different script and datum, so they're not here. */
const V2_LP_POLICY = 'f5808c2c990d86da54bfc97d89cee6efa20cd8461616359478d96b4c';

const sha3 = (hex) => createHash('sha3-256').update(Buffer.from(hex, 'hex')).digest('hex');

/** The LP token name Minswap derives for a pair — the check that a pair is the right one. */
function lpTokenName(a, b) {
  return sha3(sha3(a.currency_symbol + a.token_name) + sha3(b.currency_symbol + b.token_name));
}

async function page(searchAfter) {
  const res = await fetch(API, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      term: '',
      only_verified: false,
      limit: PAGE,
      sort_field: 'liquidity',
      sort_direction: 'desc',
      currency: 'usd',
      ...(searchAfter ? { search_after: searchAfter } : {}),
    }),
  });
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}: ${await res.text()}`);
  return res.json();
}

const assets = [];
const assetIndex = new Map();
/** Assets are interned: ADA is one side of nearly every pool, and tokens repeat across
 *  fee tiers and pool versions. */
function intern(asset) {
  const key = asset.currency_symbol + '.' + asset.token_name;
  let i = assetIndex.get(key);
  if (i === undefined) {
    i = assets.push([asset.currency_symbol, asset.token_name]) - 1;
    assetIndex.set(key, i);
  }
  return i;
}

const lp = {};
let cursor;
let seen = 0;
let otherType = 0;
let mismatched = 0;

while (Object.keys(lp).length < POOLS) {
  const body = await page(cursor);
  const batch = body.pool_metrics ?? [];
  if (batch.length === 0) break;
  for (const pool of batch) {
    seen++;
    const name = pool.lp_asset.token_name;
    // V1 and stableswap pools are a different derivation entirely — expected, not a fault.
    if (pool.lp_asset.currency_symbol !== V2_LP_POLICY) {
      otherType++;
      continue;
    }
    // Among V2 pools the hash must reproduce, so a mismatch here is a real problem and
    // says so: the wrong pair, or the right pair the wrong way round.
    if (lpTokenName(pool.asset_a, pool.asset_b) !== name) {
      mismatched++;
      console.error(`  MISMATCH ${name.slice(0, 16)}…: V2 pair does not hash to its LP name`);
      continue;
    }
    lp[name] = [intern(pool.asset_a), intern(pool.asset_b)];
  }
  cursor = body.search_after;
  if (!cursor) break;
}

writeFileSync(OUT, JSON.stringify({ policy: V2_LP_POLICY, assets, pools: lp }) + '\n');
console.error(
  `wrote ${OUT}: ${Object.keys(lp).length} V2 pools verified, ${assets.length} distinct assets ` +
    `(${otherType} of ${seen} skipped as V1/stableswap, ${mismatched} hash mismatches)`,
);
if (mismatched > 0) throw new Error('a V2 pair failed its own hash — the derivation or the source changed');
