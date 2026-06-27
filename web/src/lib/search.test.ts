import { describe, it, expect, vi, afterEach } from 'vitest';
import { sanitizeQuery, searchTarget, isHash, resolveHexTarget } from './search';

// Real mainnet addresses + valid testnet/script addresses constructed from them
// (same credential, flipped network nibble / script HRP, recomputed checksum).
const POOL = 'pool13l9hxj5dke9yd8xrxtfdkxgppj4wdyy52k99jded9f4kv5ce7al';
const DREP = 'drep1cm6mupj0lszffgnfe3t0we7r7c9kwc6lxd4ad7dstf6gggtdqc3';
const DREP_SCRIPT = 'drep_script1cm6mupj0lszffgnfe3t0we7r7c9kwc6lxd4ad7dstf6gg8fsgys';
const STAKE = 'stake1uxpnxus5gsxf43a9t6qhuztl3ljtdxyhz8ttrfy0fnexj5g4w02m6';
const STAKE_TEST = 'stake_test1uzpnxus5gsxf43a9t6qhuztl3ljtdxyhz8ttrfy0fnexj5gjy9gl8';
const ADDR = 'addr1q800ecxug9zluh2jn8xmeu5n6u70fs29y08wzekc2g92uz5rxdepg3qvntr62h5p0cyhlrlyk6vfwywkkxjg7n8jd9gsg5ajq0';
const ADDR_TEST =
  'addr_test1qr00ecxug9zluh2jn8xmeu5n6u70fs29y08wzekc2g92uz5rxdepg3qvntr62h5p0cyhlrlyk6vfwywkkxjg7n8jd9gstzqjvs';
const ASSET = 'asset1wd3llgkhsw6etxf2yca6cgk9ssrpva3wf0pq9a';
const POLICY = 'a5bb0e5bb275a573d744a021f9b3bff73595468e002755b447e01559'; // 56 hex
// A real pool hash (28 bytes) and its bech32 — same hex format as POLICY.
const POOL_HASH = 'abacadaba9f12a8b5382fc370e4e7e69421fb59831bb4ecca3a11d9b';
const POOL_BECH = 'pool14wk2m2af7y4gk5uzlsmsunn7d9ppldvcxxa5an9r5ywek8330fg';

describe('sanitizeQuery', () => {
  it('lowercases and keeps only [a-z0-9_]', () => {
    expect(sanitizeQuery('Pool1ABC')).toBe('pool1abc');
    expect(sanitizeQuery('addr_test1XYZ')).toBe('addr_test1xyz');
    expect(sanitizeQuery('a b-c.d!e/f')).toBe('abcdef');
    expect(sanitizeQuery('  STAKE_1  ')).toBe('stake_1');
  });
});

describe('searchTarget', () => {
  it('routes complete mainnet bech32 addresses to their feed', () => {
    expect(searchTarget(POOL)).toBe(`/${POOL}`);
    expect(searchTarget(DREP)).toBe(`/${DREP}`);
    expect(searchTarget(STAKE)).toBe(`/${STAKE}`);
    expect(searchTarget(ADDR)).toBe(`/${ADDR}`);
    expect(searchTarget(ASSET)).toBe(`/${ASSET}`);
  });

  it('routes testnet and script bech32 addresses', () => {
    expect(searchTarget(STAKE_TEST)).toBe(`/${STAKE_TEST}`);
    expect(searchTarget(ADDR_TEST)).toBe(`/${ADDR_TEST}`);
    expect(searchTarget(DREP_SCRIPT)).toBe(`/${DREP_SCRIPT}`);
  });

  it('does NOT route a bare 56-hex hash (ambiguous pool-hash vs policy-id)', () => {
    // Resolved by resolveHexTarget instead; searchTarget only knows bech32.
    expect(searchTarget(POLICY)).toBeNull();
    expect(searchTarget(POOL_HASH)).toBeNull();
  });

  it('sanitizes before matching (case + stray characters)', () => {
    expect(searchTarget(` ${POOL.toUpperCase()} `)).toBe(`/${POOL}`);
  });

  it('returns null while incomplete or invalid', () => {
    expect(searchTarget('')).toBeNull();
    expect(searchTarget('pool1')).toBeNull(); // partial
    expect(searchTarget(POOL.slice(0, -1) + 'q')).toBeNull(); // tampered checksum
    expect(searchTarget('notanaddress')).toBeNull();
    expect(searchTarget('asset1notavalidchecksum')).toBeNull();
  });
});

describe('isHash', () => {
  it('true for a complete 56-hex hash (case- and space-insensitive)', () => {
    expect(isHash(POLICY)).toBe(true);
    expect(isHash(POOL_HASH.toUpperCase())).toBe(true);
    expect(isHash(`  ${POOL_HASH}  `)).toBe(true);
  });

  it('false for partial, over-length, or non-hex', () => {
    expect(isHash(POLICY.slice(0, 54))).toBe(false); // 54 hex
    expect(isHash(POLICY + '00')).toBe(false); // 58 hex
    expect(isHash(POOL)).toBe(false); // bech32, not bare hex
    expect(isHash('')).toBe(false);
  });
});

describe('resolveHexTarget', () => {
  afterEach(() => vi.restoreAllMocks());

  function mockSearch(hits: unknown) {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => ({ ok: true, json: async () => hits })),
    );
  }

  it('routes a registered pool hash to its pool feed', async () => {
    mockSearch([{ id: POOL_BECH, label: 'SMAUG', kind: 'pool' }]);
    expect(await resolveHexTarget(POOL_HASH)).toBe(`/${POOL_BECH}`);
  });

  it('falls back to /policy when the hex is not a known pool', async () => {
    mockSearch([]);
    expect(await resolveHexTarget(POLICY)).toBe(`/policy/${POLICY}`);
  });

  it('falls back to /policy on a fetch error', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => {
        throw new Error('network');
      }),
    );
    expect(await resolveHexTarget(POLICY)).toBe(`/policy/${POLICY}`);
  });
});
