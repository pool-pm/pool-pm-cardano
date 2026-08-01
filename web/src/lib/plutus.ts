/**
 * Read Plutus datums.
 *
 * The server sends a script output's inline datum as hex, exactly as it appears on
 * chain, and every interpretation of it happens here — a protocol shipping a new datum
 * version then costs a frontend deploy rather than a server restart, which is the whole
 * reason the raw bytes cross the wire instead of a decoded shape.
 *
 * This is the CBOR subset PlutusData uses, and nothing more. That subset is fixed by the
 * ledger and doesn't move; what moves is the *schema* each protocol builds on top of it,
 * which lives in the per-protocol readers rather than here.
 *
 *   Constr  tag 121-127 (alternatives 0-6), tag 1280-1400 (7-127), tag 102 (any)
 *   Map     major 5
 *   List    major 4
 *   Int     major 0/1, plus tags 2/3 for bignums
 *   Bytes   major 2, definite or chunked
 */

export type PlutusData =
  | { kind: 'constr'; tag: number; fields: PlutusData[] }
  | { kind: 'map'; entries: [PlutusData, PlutusData][] }
  | { kind: 'list'; items: PlutusData[] }
  | { kind: 'int'; value: bigint }
  | { kind: 'bytes'; hex: string };

/** CBOR tag ranges that encode a constructor alternative. */
const CONSTR_LOW = 121; // alternatives 0-6
const CONSTR_LOW_END = 127;
const CONSTR_HIGH = 1280; // alternatives 7-127
const CONSTR_HIGH_END = 1400;
const CONSTR_ANY = 102; // [alternative, fields]
const BIGNUM_POSITIVE = 2;
const BIGNUM_NEGATIVE = 3;
/** A CBOR length of 31 in any major type means "indefinite, read until break". */
const INDEFINITE = 31;
const BREAK = 0xff;

class Reader {
  private at = 0;

  constructor(private readonly bytes: Uint8Array) {}

  get done(): boolean {
    return this.at >= this.bytes.length;
  }

  private byte(): number {
    if (this.at >= this.bytes.length) throw new Error('datum ended mid-value');
    return this.bytes[this.at++];
  }

  private uint(bytes: number): bigint {
    let value = 0n;
    for (let i = 0; i < bytes; i++) value = (value << 8n) | BigInt(this.byte());
    return value;
  }

  /** The argument of a CBOR head, or null for an indefinite length. */
  private argument(info: number): bigint | null {
    if (info < 24) return BigInt(info);
    if (info === 24) return this.uint(1);
    if (info === 25) return this.uint(2);
    if (info === 26) return this.uint(4);
    if (info === 27) return this.uint(8);
    if (info === INDEFINITE) return null;
    throw new Error(`reserved CBOR length ${info}`);
  }

  /** Read items until the break marker that closes an indefinite-length container. */
  private untilBreak<T>(read: () => T): T[] {
    const items: T[] = [];
    while (this.bytes[this.at] !== BREAK) items.push(read());
    this.at++; // consume the break
    return items;
  }

  private counted<T>(length: bigint | null, read: () => T): T[] {
    if (length === null) return this.untilBreak(read);
    const items: T[] = [];
    for (let i = 0n; i < length; i++) items.push(read());
    return items;
  }

  private bytesValue(length: bigint | null): string {
    // A chunked byte string is one value split across segments; the pieces concatenate.
    if (length === null) return this.untilBreak(() => this.readBytesChunk()).join('');
    return this.readBytesChunk(Number(length));
  }

  private readBytesChunk(length?: number): string {
    if (length === undefined) {
      const head = this.byte();
      length = Number(this.argument(head & 0x1f)!);
    }
    let hex = '';
    for (let i = 0; i < length; i++) hex += this.byte().toString(16).padStart(2, '0');
    return hex;
  }

  read(): PlutusData {
    const head = this.byte();
    const major = head >> 5;
    const info = head & 0x1f;

    switch (major) {
      case 0:
        return { kind: 'int', value: this.argument(info)! };
      case 1:
        // Negative integers encode -(n+1).
        return { kind: 'int', value: -1n - this.argument(info)! };
      case 2:
        return { kind: 'bytes', hex: this.bytesValue(this.argument(info)) };
      case 4:
        return { kind: 'list', items: this.counted(this.argument(info), () => this.read()) };
      case 5:
        return {
          kind: 'map',
          entries: this.counted(this.argument(info), () => [this.read(), this.read()] as [PlutusData, PlutusData]),
        };
      case 6:
        return this.tagged(Number(this.argument(info)!));
      default:
        throw new Error(`unsupported CBOR major type ${major}`);
    }
  }

  private tagged(tag: number): PlutusData {
    if (tag === BIGNUM_POSITIVE || tag === BIGNUM_NEGATIVE) {
      const inner = this.read();
      if (inner.kind !== 'bytes') throw new Error('bignum without bytes');
      const magnitude = inner.hex === '' ? 0n : BigInt('0x' + inner.hex);
      return { kind: 'int', value: tag === BIGNUM_POSITIVE ? magnitude : -1n - magnitude };
    }
    if (tag >= CONSTR_LOW && tag <= CONSTR_LOW_END) {
      return { kind: 'constr', tag: tag - CONSTR_LOW, fields: this.fields() };
    }
    if (tag >= CONSTR_HIGH && tag <= CONSTR_HIGH_END) {
      return { kind: 'constr', tag: tag - CONSTR_HIGH + 7, fields: this.fields() };
    }
    if (tag === CONSTR_ANY) {
      // [alternative, [fields]] — the escape hatch for alternatives past 127.
      const head = this.byte();
      this.argument(head & 0x1f);
      const alternative = this.read();
      if (alternative.kind !== 'int') throw new Error('constr alternative is not an integer');
      return { kind: 'constr', tag: Number(alternative.value), fields: this.fields() };
    }
    throw new Error(`unsupported CBOR tag ${tag}`);
  }

  private fields(): PlutusData[] {
    const head = this.byte();
    if (head >> 5 !== 4) throw new Error('constructor fields are not a list');
    return this.counted(this.argument(head & 0x1f), () => this.read());
  }
}

function toBytes(hex: string): Uint8Array {
  // Validated rather than trusted: `parseInt` yields NaN on a non-hex pair and a typed
  // array coerces that silently to 0, so garbage would decode as a plausible datum
  // instead of being rejected.
  if (hex.length % 2 !== 0 || !/^[0-9a-fA-F]*$/.test(hex)) throw new Error('not hex');
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

/**
 * Parse a datum, or null if it isn't readable.
 *
 * Null rather than throwing: a datum is arbitrary on-chain data written by anyone, so a
 * shape this doesn't handle is an ordinary outcome, not an error — and it must never
 * take a tile down with it.
 */
export function parseDatum(hex: string | undefined): PlutusData | null {
  if (!hex) return null;
  try {
    return new Reader(toBytes(hex)).read();
  } catch {
    return null;
  }
}

// --- Navigation ---
//
// Per-protocol readers walk a datum by shape. These keep that walking total: a missing
// or mistyped field is undefined, never a crash, because a datum's shape is only ever a
// guess about what a protocol wrote. They accept null so `asInt(parseDatum(hex))`
// composes without the caller unwrapping first.

/** The `index`th field of `data`, when it's a constructor with that many fields. */
export function field(data: PlutusData | null | undefined, index: number): PlutusData | undefined {
  return data?.kind === 'constr' ? data.fields[index] : undefined;
}

/** Follow a chain of field indices: `path(datum, 6, 1, 0)`. */
export function path(data: PlutusData | null | undefined, ...indices: number[]): PlutusData | undefined {
  return indices.reduce<PlutusData | undefined>((node, i) => field(node, i), data ?? undefined);
}

/** The constructor alternative, for datums that branch on it. */
export function constrTag(data: PlutusData | null | undefined): number | undefined {
  return data?.kind === 'constr' ? data.tag : undefined;
}

export function asInt(data: PlutusData | null | undefined): bigint | undefined {
  return data?.kind === 'int' ? data.value : undefined;
}

export function asBytes(data: PlutusData | null | undefined): string | undefined {
  return data?.kind === 'bytes' ? data.hex : undefined;
}
