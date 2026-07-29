import { describe, it, expect } from 'vitest';
import { formatQuantity, formatVotes } from './layout';

// Compare against the same Intl call so the assertions don't hardcode a locale separator.
const group = (n: string) => new Intl.NumberFormat().format(BigInt(n));

describe('formatQuantity', () => {
  it('groups the integer part and preserves the fraction', () => {
    expect(formatQuantity('520000')).toBe(group('520000'));
    expect(formatQuantity('1204.55')).toBe(group('1204') + '.55');
    expect(formatQuantity('0.000001')).toBe(group('0') + '.000001');
  });

  it('leaves small numbers unchanged', () => {
    expect(formatQuantity('42')).toBe('42');
    expect(formatQuantity('7.5')).toBe('7.5');
  });

  it('stays exact past Number.MAX_SAFE_INTEGER (huge token supplies)', () => {
    const big = '123456789012345678901234567890';
    expect(formatQuantity(big)).toBe(group(big));
    // grouping actually happened — the digits are unchanged but separators were added
    expect(formatQuantity(big).replace(/\D/g, '')).toBe(big);
    expect(formatQuantity(big).length).toBeGreaterThan(big.length);
  });

  it('handles a leading minus sign', () => {
    expect(formatQuantity('-1000')).toBe('-' + group('1000'));
  });

  it('leaves non-numeric input untouched', () => {
    expect(formatQuantity('abc')).toBe('abc');
    expect(formatQuantity('')).toBe('');
  });
});

describe('formatVotes', () => {
  it('appends the participation percentage', () => {
    expect(formatVotes(148, 151)).toBe('148 votes (98%)');
    expect(formatVotes(4, 27)).toBe('4 votes (15%)');
    expect(formatVotes(1, 1)).toBe('1 vote (100%)');
  });

  it('omits the percentage when the denominator is unknown or nothing was voted', () => {
    expect(formatVotes(12)).toBe('12 votes');
    expect(formatVotes(12, 0)).toBe('12 votes');
    // 0 votes reads better bare than as "0 votes (0%)".
    expect(formatVotes(0, 151)).toBe('0 votes');
  });

  it('clamps at 100%, since the denominator only refreshes at epoch boundaries', () => {
    expect(formatVotes(152, 151)).toBe('152 votes (100%)');
  });
});
