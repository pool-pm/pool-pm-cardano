import { describe, it, expect } from 'vitest';
import { sanitizeQuery, searchTarget } from './search';

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

  it('routes a 56-hex policy id to /policy', () => {
    expect(searchTarget(POLICY)).toBe(`/policy/${POLICY}`);
  });

  it('sanitizes before matching (case + stray characters)', () => {
    expect(searchTarget(` ${POOL.toUpperCase()} `)).toBe(`/${POOL}`);
    expect(searchTarget(POLICY.toUpperCase())).toBe(`/policy/${POLICY}`);
  });

  it('returns null while incomplete or invalid', () => {
    expect(searchTarget('')).toBeNull();
    expect(searchTarget('pool1')).toBeNull(); // partial
    expect(searchTarget(POOL.slice(0, -1) + 'q')).toBeNull(); // tampered checksum
    expect(searchTarget(POLICY.slice(0, 54))).toBeNull(); // 54 hex
    expect(searchTarget(POLICY + '00')).toBeNull(); // 58 hex
    expect(searchTarget('notanaddress')).toBeNull();
    expect(searchTarget('asset1notavalidchecksum')).toBeNull();
  });
});
