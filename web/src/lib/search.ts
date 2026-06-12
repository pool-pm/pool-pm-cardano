import { bech32Hrp } from './bech32';
import type { SearchResult } from './types';

// bech32 prefixes that map to a feed/page (testnet variants carry an underscore).
const FEED_HRPS = new Set(['pool', 'drep', 'drep_script', 'stake', 'stake_test', 'addr', 'addr_test', 'asset']);

// Keep only lowercased letters, digits and underscore — the charset of a bech32
// address (HRP + data) or a hex policy id; everything else is dropped.
export function sanitizeQuery(raw: string): string {
  return raw.toLowerCase().replace(/[^a-z0-9_]/g, '');
}

// The page to navigate to once the field holds a *complete* address: a 56-hex
// policy id (`/policy/…`) or a valid-checksum bech32 address of a known prefix
// (`/…`, routed to its feed/page by App.svelte). Returns null while incomplete.
export function searchTarget(raw: string): string | null {
  const v = sanitizeQuery(raw);
  if (/^[0-9a-f]{56}$/.test(v)) return `/policy/${v}`;
  const hrp = bech32Hrp(v);
  return hrp && FEED_HRPS.has(hrp) ? `/${v}` : null;
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
