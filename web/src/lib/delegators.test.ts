import { describe, it, expect } from 'vitest';
import { matchesFilter, shortStake } from './delegators';

const addr = 'stake1u8xk7jpvmgd0vmzsnjc0e3f2m4qkgqpz5pnn7ptzq0j6q7fz9abcd';

describe('shortStake', () => {
  it('elides the middle of a long address and leaves a short one alone', () => {
    expect(shortStake(addr)).toBe(`${addr.slice(0, 11)}…${addr.slice(-6)}`);
    expect(shortStake('stake1short')).toBe('stake1short');
  });
});

describe('matchesFilter', () => {
  const withHandle = { handle: 'Alice', stake_address: addr };
  const noHandle = { stake_address: addr };

  it('matches everything when the filter is empty', () => {
    expect(matchesFilter(noHandle, '')).toBe(true);
    expect(matchesFilter(noHandle, '   ')).toBe(true);
  });

  it('matches a handle substring case-insensitively', () => {
    expect(matchesFilter(withHandle, 'LIC')).toBe(true);
    expect(matchesFilter(withHandle, 'bob')).toBe(false);
    expect(matchesFilter(noHandle, 'alice')).toBe(false);
  });

  it('treats a leading $ as handle-only', () => {
    expect(matchesFilter(withHandle, '$alice')).toBe(true);
    expect(matchesFilter(noHandle, '$alice')).toBe(false);
    // A bare `$` is not a filter (the server reads it as absent), so it excludes nothing.
    expect(matchesFilter(noHandle, '$')).toBe(true);
  });

  it('matches an address prefix', () => {
    expect(matchesFilter(noHandle, addr.slice(0, 18))).toBe(true);
    expect(matchesFilter(noHandle, addr)).toBe(true);
    expect(matchesFilter(noHandle, 'stake1uzzzz')).toBe(false);
  });
});
