import { describe, it, expect } from 'vitest';
import { formatQuantity } from './layout';

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
