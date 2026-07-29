import type { Delegator } from './types';

/** Compact a stake address for a tile's bottom band: stake1u8x…6q7fz9. */
export function shortStake(addr: string): string {
  return addr.length > 20 ? `${addr.slice(0, 11)}…${addr.slice(-6)}` : addr;
}

/**
 * Client mirror of the server's `?q=` rule, applied to *live* additions only (a page
 * fetch is already filtered server-side): `$name` or a bare word matches the handle,
 * anything starting with `stake` matches the address as a prefix.
 */
export function matchesFilter(d: Pick<Delegator, 'handle' | 'stake_address'>, q: string): boolean {
  const needle = q.trim().toLowerCase();
  if (!needle) return true;
  if (needle.startsWith('$')) {
    const name = needle.slice(1);
    // A bare `$` isn't a filter (the server treats it as absent), so nothing is excluded.
    return !name || !!d.handle?.toLowerCase().includes(name);
  }
  if (needle.startsWith('stake')) return d.stake_address.startsWith(needle);
  return !!d.handle?.toLowerCase().includes(needle);
}
