import { describe, it, expect } from 'vitest';
import { asBytes, asInt, constrTag, field, parseDatum, path, type PlutusData } from './plutus';

/**
 * A real Minswap V2 order datum, taken verbatim from chain. The expected shape below is
 * db-sync's own decoding of the same bytes, so this checks the reader against an
 * independent implementation rather than against itself.
 */
const MINSWAP_ORDER =
  'd8799fd8799f581c636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5affd8799fd8799f581c636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5affd8799fd8799fd8799f581ce39b5f40aa85fbc121a625d777a776eca1cb4c923426949c997d8828ffffffffd87980d8799fd8799f581c636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5affd8799fd8799fd8799f581ce39b5f40aa85fbc121a625d777a776eca1cb4c923426949c997d8828ffffffffd87980d8799f581cf5808c2c990d86da54bfc97d89cee6efa20cd8461616359478d96b4c5820686db0c143a3a2cc19099d8909e315c4ed761a6ac5a3c5998c651d5e9d3cb253ffd8799fd87a80d8799f1a26ef03a4ff1adfaf40f4d87980ff1a001e8480d87a80ff';

describe('parseDatum: a real order datum', () => {
  const datum = parseDatum(MINSWAP_ORDER)!;

  it('reads the outer constructor', () => {
    expect(datum.kind).toBe('constr');
    expect(constrTag(datum)).toBe(0);
    expect((datum as { fields: PlutusData[] }).fields).toHaveLength(9);
  });

  it('reads the owner credential', () => {
    expect(asBytes(path(datum, 0, 0))).toBe('636d0d0118a8933ac167d4c448150bb325deaf7a4fdfb44adc7f2f5a');
  });

  it('reads the LP asset that names the pool', () => {
    expect(asBytes(path(datum, 5, 0))).toBe('f5808c2c990d86da54bfc97d89cee6efa20cd8461616359478d96b4c');
    expect(asBytes(path(datum, 5, 1))).toBe('686db0c143a3a2cc19099d8909e315c4ed761a6ac5a3c5998c651d5e9d3cb253');
  });

  it('reads the swap step: direction, amount in, minimum out', () => {
    const step = field(datum, 6)!;
    // Constr 1 on the direction field — B→A for this order.
    expect(constrTag(field(step, 0))).toBe(1);
    expect(asInt(path(step, 1, 0))).toBe(653198244n);
    expect(asInt(field(step, 2))).toBe(3752804596n);
  });

  it('reads the batcher fee', () => {
    expect(asInt(field(datum, 7))).toBe(2000000n);
  });
});

describe('parseDatum: CBOR shapes', () => {
  it('reads a definite-length constructor', () => {
    // d87982 = tag 121, array(2); 01 02
    expect(parseDatum('d879820102')).toEqual({
      kind: 'constr',
      tag: 0,
      fields: [
        { kind: 'int', value: 1n },
        { kind: 'int', value: 2n },
      ],
    });
  });

  it('reads alternatives past the first tag range', () => {
    // d9050080 = tag 1280, array(0) -> alternative 7
    expect(constrTag(parseDatum('d9050080'))).toBe(7);
  });

  it('reads a negative integer', () => {
    expect(asInt(parseDatum('20'))).toBe(-1n);
  });

  it('reads a bignum too large for a machine integer', () => {
    // tag 2 (positive bignum), 9 bytes: 2^64
    expect(asInt(parseDatum('c249010000000000000000'))).toBe(18446744073709551616n);
  });

  it('reads a map', () => {
    // a1 00 41ff = {0: h'ff'}
    const map = parseDatum('a10041ff')!;
    expect(map.kind).toBe('map');
    expect(map.kind === 'map' && asInt(map.entries[0][0])).toBe(0n);
    expect(map.kind === 'map' && asBytes(map.entries[0][1])).toBe('ff');
  });

  it('reads a chunked byte string as one value', () => {
    // 5f 41aa 41bb ff — indefinite bytes in two chunks
    expect(asBytes(parseDatum('5f41aa41bbff'))).toBe('aabb');
  });
});

describe('parseDatum: bad input', () => {
  // A datum is arbitrary data written by anyone, so unreadable is an ordinary outcome —
  // it must produce null rather than take a tile down.
  it.each([
    ['', 'empty'],
    ['zz', 'not hex'],
    ['d879', 'truncated'],
    ['ff', 'a bare break'],
  ])('returns null for %j (%s)', (hex) => {
    expect(parseDatum(hex)).toBeNull();
  });

  it('returns null for undefined', () => {
    expect(parseDatum(undefined)).toBeNull();
  });
});

describe('navigation', () => {
  const datum = parseDatum('d879820102')!;

  it('is total on a missing field', () => {
    expect(field(datum, 99)).toBeUndefined();
    expect(path(datum, 0, 0, 0)).toBeUndefined();
    expect(asInt(field(datum, 99))).toBeUndefined();
  });

  it('is total on the wrong type', () => {
    expect(asBytes(field(datum, 0))).toBeUndefined();
    expect(constrTag(field(datum, 0))).toBeUndefined();
  });
});
