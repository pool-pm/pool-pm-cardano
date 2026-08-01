import { describe, it, expect } from 'vitest';
import { messageLines, metadataLines } from './metadata';
import type { MetadataEntry, MetadataValue } from './types';

const map = (pairs: [string, MetadataValue][]): MetadataValue => ({
  map: pairs.map(([k, v]) => ({ k, v })),
});
const entry = (label: number, value: MetadataValue): MetadataEntry[] => [{ label, value }];

describe('messageLines', () => {
  it('reads CIP-20 text, which is where dApps name themselves', () => {
    expect(messageLines(entry(674, map([['msg', ['Minswap: Market Order']]])))).toEqual(['Minswap: Market Order']);
  });

  it('accepts a single string as well as an array', () => {
    expect(messageLines(entry(674, map([['msg', 'thanks']])))).toEqual(['thanks']);
  });

  it('ignores non-text entries in the array', () => {
    expect(messageLines(entry(674, map([['msg', ['ok', 42, { bytes: 'ff' }]]])))).toEqual(['ok']);
  });

  it('has nothing to say without a 674 label', () => {
    expect(messageLines(entry(1, map([['timestamp', 1]])))).toEqual([]);
    expect(messageLines(undefined)).toEqual([]);
  });
});

describe('metadataLines', () => {
  it('shows an unknown label’s values, not just its key names', () => {
    // This is the whole point of sending metadata as data. Label 1 rides ~9,800 txs a
    // month; the server used to send "timestamp absolute_slot" with the values dropped.
    expect(
      metadataLines(
        entry(
          1,
          map([
            ['timestamp', 1785570471],
            ['absolute_slot', 194004180],
          ]),
        ),
      ),
    ).toEqual(['timestamp 1785570471', 'absolute_slot 194004180']);
  });

  it('names a key whose value is too long to read on a tile', () => {
    const long = 'a'.repeat(80);
    expect(metadataLines(entry(8746, map([['root', long]])))).toEqual(['root']);
  });

  it('caps how many keys it summarises', () => {
    expect(
      metadataLines(
        entry(
          99,
          map([
            ['a', 1],
            ['b', 2],
            ['c', 3],
            ['d', 4],
          ]),
        ),
      ),
    ).toEqual(['a 1', 'b 2', 'c 3']);
  });

  it('falls back to the label when nothing in it reads', () => {
    expect(metadataLines(entry(100, map([[{ bytes: '00' } as never, 'x']])))).toEqual(['metadata 100']);
    expect(metadataLines(entry(0, { bytes: '0000' }))).toEqual(['metadata 0']);
  });

  it('names SundaeSwap governance by its label', () => {
    expect(metadataLines(entry(31415, map([['x', 1]])))).toEqual(['SundaeSwap governance']);
  });

  it('does not mistake a list for a map', () => {
    // Every array has a `map` method, so a careless structural check reads a metadata
    // list as a map and hands back the method.
    expect(metadataLines(entry(42, ['one', 'two']))).toEqual(['metadata 42']);
  });

  it('renders each label in turn', () => {
    expect(
      metadataLines([
        { label: 674, value: map([['msg', ['hello']]]) },
        { label: 1, value: map([['timestamp', 7]]) },
      ]),
    ).toEqual(['hello', 'timestamp 7']);
  });
});
