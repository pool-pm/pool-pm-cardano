// Regenerate `src/lib/dapps.json` from the CRFA off-chain data registry.
//
//   node scripts/build-dapps.mjs
//
// The registry (github.com/Cardano-Fans/crfa-offchain-data-registry) has one file per
// dApp, each listing its scripts with a human `name`, a `purpose`, and one entry per
// deployed version carrying either a `contractAddress` (SPEND) or a `mintPolicyID`
// (MINT). We keep three things per script address / mint policy:
//
//   - the project name          → "MINSWAP"
//   - its category/subCategory  → tells a DEX from a lending market from a marketplace
//   - a canonical *role*        → what the script is for ("order", "pool", "farm", …)
//
// The role is what lets a tx read as `$bob SWAPPED 100 ₳ ON MINSWAP` without decoding
// the order datum: an output to Minswap's "Batch Order" script *is* an order. Raw script
// names are far too varied to use directly (874 distinct ones, e.g. 236 numbered
// "VyFi: LP Order Process N"), so they're folded into the small vocabulary in ROLES.

import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = 'Cardano-Fans/crfa-offchain-data-registry';
const OUT = join(dirname(fileURLToPath(import.meta.url)), '..', 'src', 'lib', 'dapps.json');

/**
 * Script name → canonical role. First match wins, so the order is the specificity
 * order: "Deposit (Weighted Pool)" is a deposit, not a pool; "Vault Harvest" is a
 * harvest, not a vault.
 */
const ROLES = [
  ['deposit', /deposit|zap/],
  ['redeem', /redeem|unstake|withdraw|cancel|flush/],
  ['order', /order|request|swap|batch|dex|escrow/],
  ['farm', /farm|harvest|yield|reward/],
  ['stake', /stak|delegat/],
  ['lend', /lend|borrow|cdp|collateral|loan|liquidat/],
  ['vault', /vault|bar\b|treasury|reserve/],
  ['pool', /pool|liquidity|amm|factory|lppolicy/],
  ['market', /market|listing|sale|auction|offer|bid/],
  ['vesting', /vest|lock|claim|airdrop|distribut/],
  ['oracle', /oracle|feed|price/],
  ['launchpad', /launchpad|ido|mint/],
  ['governance', /govern|vote|poll|proposal|dao/],
];

/**
 * Registry project names that don't work as a display label. The catch-all bucket the
 * registry uses for shared/unattributed scripts has a sentence for a name — it would be
 * a paragraph once uppercased into a tx tile.
 */
const RENAME = {
  "Smart contracts that are leveraged by multiple projects and shouldn't be credited to a specific one":
    'Shared contract',
};

function roleOf(name) {
  if (!name) return null;
  const n = name.toLowerCase();
  for (const [role, re] of ROLES) if (re.test(n)) return role;
  return null;
}

/**
 * Sources beyond the registry: a protocol's own published constants.
 *
 * CRFA describes contract versions that are largely no longer the ones in use — its
 * newest Minswap order script sees ~75 outputs a day while the live one sees ~5,600 —
 * and a protocol that open-sources its SDK is the authority on its own addresses. The
 * constants are plain `name: "value"` pairs, so the key names double as role labels
 * (`orderScriptHash` → order, `poolCreationAddress` → pool).
 *
 * Testnet entries come along harmlessly: a testnet address never matches a mainnet one,
 * and a 28-byte script hash won't collide.
 */
const SUPPLEMENTS = [
  { app: 'Minswap', url: 'https://raw.githubusercontent.com/minswap/sdk/main/src/types/constants.ts' },
];

/** `key: "value"` pairs from a TypeScript constants file, across line breaks. */
function constantPairs(source) {
  const pairs = [];
  const re = /([A-Za-z_][\w]*)\s*:\s*\n?\s*"([^"\n]+)"/g;
  for (let m = re.exec(source); m !== null; m = re.exec(source)) pairs.push([m[1], m[2]]);
  return pairs;
}

async function text(url) {
  const res = await fetch(url, { headers: { 'user-agent': 'pool-pm-dapps' } });
  if (!res.ok) throw new Error(`${res.status} ${res.statusText} for ${url}`);
  return res.text();
}

async function json(url) {
  const res = await fetch(url, {
    headers: { accept: 'application/vnd.github.raw+json', 'user-agent': 'pool-pm-dapps' },
  });
  if (!res.ok) throw new Error(`${res.status} ${res.statusText} for ${url}`);
  return res.json();
}

const tree = await json(`https://api.github.com/repos/${REPO}/git/trees/main?recursive=1`);
const files = tree.tree.filter((e) => e.path.startsWith('dApps/') && e.path.endsWith('.json')).map((e) => e.path);
if (files.length === 0) throw new Error('no dApp files found — did the registry layout change?');
console.error(`fetching ${files.length} dApp files…`);

const apps = [];
const roles = [];
const addr = {};
/** Script hash (28-byte hex) → entry. Matches a script under every address form it's
 *  deployed at, which is what an address list can't do. */
const hash = {};
const policy = {};

const roleIndex = (role) => {
  if (role === null) return -1;
  let i = roles.indexOf(role);
  if (i === -1) i = roles.push(role) - 1;
  return i;
};

for (const path of files) {
  let d;
  try {
    d = await json(`https://raw.githubusercontent.com/${REPO}/main/${path}`);
  } catch (e) {
    console.error(`  skipped ${path}: ${e.message}`);
    continue;
  }
  const name = RENAME[d.projectName] ?? d.projectName;
  if (!name) continue;
  const app = apps.push({ name, category: d.category ?? null, sub: d.subCategory ?? null }) - 1;

  for (const script of d.scripts ?? []) {
    const role = roleIndex(roleOf(script.name));
    const entry = role === -1 ? [app] : [app, role];
    for (const v of script.versions ?? []) {
      // A script address may appear under several versions (and, rarely, under two
      // scripts of the same project); first writer wins so the earliest — usually the
      // better-named — role sticks.
      if (v.contractAddress && !(v.contractAddress in addr)) addr[v.contractAddress] = entry;
      if (v.mintPolicyID && !(v.mintPolicyID in policy)) policy[v.mintPolicyID] = entry;
    }
  }
}

for (const supplement of SUPPLEMENTS) {
  let source;
  try {
    source = await text(supplement.url);
  } catch (e) {
    console.error(`  skipped ${supplement.app} constants: ${e.message}`);
    continue;
  }
  // Reuse the app's registry entry when it has one, so a supplement never splits a
  // project into two.
  let app = apps.findIndex((a) => a.name === supplement.app);
  if (app === -1) app = apps.push({ name: supplement.app, category: null, sub: null }) - 1;

  let added = 0;
  for (const [key, value] of constantPairs(source)) {
    const role = roleIndex(roleOf(key));
    const entry = role === -1 ? [app] : [app, role];
    // A supplement is the protocol's own word, so it overrides the registry's guess.
    if (/^(addr|stake)(_test)?1[a-z0-9]{20,}$/.test(value)) {
      addr[value] = entry;
      added++;
    } else if (/^[0-9a-f]{56}$/.test(value)) {
      // 28 bytes: a script hash, or a policy id — which is also a script hash, so both
      // maps get it and whichever lookup asks first wins.
      hash[value] = entry;
      if (!(value in policy)) policy[value] = entry;
      added++;
    }
  }
  console.error(`  ${supplement.app}: +${added} entries from its own constants`);
}

mkdirSync(dirname(OUT), { recursive: true });
writeFileSync(OUT, JSON.stringify({ apps, roles, addr, hash, policy }) + '\n');
console.error(
  `wrote ${OUT}: ${apps.length} apps, ${roles.length} roles, ${Object.keys(addr).length} addresses, ` +
    `${Object.keys(hash).length} script hashes, ${Object.keys(policy).length} mint policies`,
);
