import { bech32Hrp } from './bech32';
import type { SearchResult } from './types';

// bech32 prefixes that map to a feed/page (testnet variants carry an underscore).
const FEED_HRPS = new Set(['pool', 'drep', 'drep_script', 'stake', 'stake_test', 'addr', 'addr_test', 'asset']);

// Keep only lowercased letters, digits and underscore — the charset of a bech32
// address (HRP + data) or a hex policy id; everything else is dropped.
export function sanitizeQuery(raw: string): string {
  return raw.toLowerCase().replace(/[^a-z0-9_]/g, '');
}

// A complete 28-byte hash in hex — a raw pool hash or a minting policy id (same
// format). `isHash` gates the async resolver below; `searchTarget` deliberately no
// longer routes it (the two can't be told apart without the pool registry).
const HASH_HEX = /^[0-9a-f]{56}$/;

export function isHash(raw: string): boolean {
  return HASH_HEX.test(sanitizeQuery(raw));
}

// The page to navigate to once the field holds a *complete* valid-checksum bech32
// address of a known prefix (`/…`, routed to its feed/page by App.svelte). Returns
// null while incomplete. A bare 56-hex hash is resolved by `resolveHexTarget`, not
// here, because a pool hash and a policy id share the same format.
export function searchTarget(raw: string): string | null {
  const v = sanitizeQuery(raw);
  const hrp = bech32Hrp(v);
  return hrp && FEED_HRPS.has(hrp) ? `/${v}` : null;
}

// Resolve an ambiguous 56-hex hash to its destination: if the server recognizes it
// as a registered pool, the pool feed (`/{pool-bech32}`); otherwise treat it as a
// minting policy id (`/policy/{hex}`). Falls back to the policy page on any error.
export async function resolveHexTarget(raw: string): Promise<string> {
  const hex = sanitizeQuery(raw);
  try {
    const res = await fetch(`/api/search?q=${hex}`);
    if (res.ok) {
      const hits = (await res.json()) as SearchResult[];
      const pool = hits.find((h) => h.kind === 'pool');
      if (pool) return `/${pool.id}`;
    }
  } catch {
    /* fall through to the policy page */
  }
  return `/policy/${hex}`;
}

// Fuzzy suggestions for a partial pool ticker / DRep name, ranked server-side by
// string distance. Returns [] for short queries or on error.
export async function searchSuggestions(raw: string): Promise<SearchResult[]> {
  const q = raw.trim();
  if (q.length < 2) return [];
  try {
    const res = await fetch(`/api/search?q=${encodeURIComponent(q)}`);
    if (!res.ok) return [];
    return (await res.json()) as SearchResult[];
  } catch {
    return [];
  }
}
