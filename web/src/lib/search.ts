import { bech32Hrp } from './bech32';
import type { SearchResult } from './types';

// bech32 prefixes that map to a feed/page (testnet variants carry an underscore).
const FEED_HRPS = new Set(['pool', 'drep', 'drep_script', 'stake', 'stake_test', 'addr', 'addr_test', 'asset']);

// The two ledger-level DRep options. They have feeds like any DRep (the server resolves
// both ids, and Always Abstain is the chain's largest delegation bucket) but they aren't
// bech32, so the checksum test below can't recognise them — they need naming explicitly.
export const PREDEFINED_DREP_IDS = new Set(['drep_always_abstain', 'drep_always_no_confidence']);

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
  if (PREDEFINED_DREP_IDS.has(v)) return `/${v}`;
  const hrp = bech32Hrp(v);
  return hrp && FEED_HRPS.has(hrp) ? `/${v}` : null;
}

// Whether a URL path is a real feed subject: the root (homepage), or a complete
// valid-checksum bech32 of a known feed prefix (addr/stake/pool/drep/asset). Used by the
// router to send anything else to the Not Found page instead of a dead SSE connection.
export function isFeedPath(path: string): boolean {
  if (path === '') return true;
  if (PREDEFINED_DREP_IDS.has(path)) return true;
  const hrp = bech32Hrp(path);
  return hrp !== null && FEED_HRPS.has(hrp);
}

// The subject of a `/<pool|drep>/delegators` path, or null if that's not what this is.
// Lives here (not inline in App.svelte) so the same "don't forget the non-bech32 DRep ids"
// rule as `isFeedPath` is unit-tested rather than restated in a regex.
export function delegatorsSubject(path: string): string | null {
  const subject = path.endsWith('/delegators') ? path.slice(0, -'/delegators'.length) : null;
  if (!subject) return null;
  if (PREDEFINED_DREP_IDS.has(subject)) return subject;
  const hrp = bech32Hrp(subject);
  return hrp && (hrp === 'pool' || hrp === 'drep' || hrp === 'drep_script') ? subject : null;
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
