const BECH32_ALPHABET = 'qpzry9x8gf2tvdw0s3jn54khce6mua7l';
const ALPHABET_MAP = new Map([...BECH32_ALPHABET].map((c, i) => [c, i]));

const BECH32_GEN = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];

function polymod(values: number[]): number {
  let chk = 1;
  for (const v of values) {
    const top = chk >>> 25;
    chk = ((chk & 0x1ffffff) << 5) ^ v;
    for (let i = 0; i < 5; i++) if ((top >>> i) & 1) chk ^= BECH32_GEN[i];
  }
  return chk;
}

function hrpExpand(hrp: string): number[] {
  const out: number[] = [];
  for (let i = 0; i < hrp.length; i++) out.push(hrp.charCodeAt(i) >>> 5);
  out.push(0);
  for (let i = 0; i < hrp.length; i++) out.push(hrp.charCodeAt(i) & 31);
  return out;
}

// The human-readable prefix if `addr` is a valid bech32 string (checksum
// verified), else null. Used to detect a complete address. No 90-char cap —
// Cardano (CIP-5) bech32 addresses exceed it. Assumes lowercase input.
export function bech32Hrp(addr: string): string | null {
  const sep = addr.lastIndexOf('1');
  if (sep < 1 || addr.length - sep - 1 < 6) return null;
  const hrp = addr.slice(0, sep);
  const values: number[] = [];
  for (const c of addr.slice(sep + 1)) {
    const v = ALPHABET_MAP.get(c);
    if (v === undefined) return null;
    values.push(v);
  }
  return polymod([...hrpExpand(hrp), ...values]) === 1 ? hrp : null;
}

export function bech32Decode(addr: string): Uint8Array | null {
  const sepIdx = addr.lastIndexOf('1');
  if (sepIdx < 1) return null;
  const data = addr.slice(sepIdx + 1).toLowerCase();

  // Convert from bech32 to 5-bit values
  const values: number[] = [];
  for (const c of data) {
    const v = ALPHABET_MAP.get(c);
    if (v === undefined) return null;
    values.push(v);
  }

  // Remove checksum (last 6 values)
  const payload = values.slice(0, -6);

  // Convert 5-bit to 8-bit
  let acc = 0;
  let bits = 0;
  const bytes: number[] = [];
  for (const v of payload) {
    acc = (acc << 5) | v;
    bits += 5;
    if (bits >= 8) {
      bits -= 8;
      bytes.push((acc >> bits) & 0xff);
    }
  }
  return new Uint8Array(bytes);
}

// Extract payment credential (28 bytes after header) as hex
export function paymentCredential(addr: string): string | null {
  const bytes = bech32Decode(addr);
  if (!bytes || bytes.length < 29) return null;
  // Header is byte 0, payment credential is bytes 1-28
  return Array.from(bytes.slice(1, 29))
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('');
}

// Extract stake credential (bytes 29-56) as hex
export function stakeCredential(addr: string): string | null {
  const bytes = bech32Decode(addr);
  if (!bytes || bytes.length < 57) return null;
  return Array.from(bytes.slice(29, 57))
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('');
}

// Extract the stake credential (28 bytes after the header) from a reward
// (stake1…) address as hex — comparable to stakeCredential() of a payment address.
export function rewardCredential(stakeAddr: string): string | null {
  const bytes = bech32Decode(stakeAddr);
  if (!bytes || bytes.length < 29) return null;
  return Array.from(bytes.slice(1, 29))
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('');
}

// --- Encode (for deriving a stake address from a payment address) ---

function bech32Checksum(hrp: string, data: number[]): number[] {
  const values = [...hrpExpand(hrp), ...data, 0, 0, 0, 0, 0, 0];
  const mod = polymod(values) ^ 1;
  const out: number[] = [];
  for (let i = 0; i < 6; i++) out.push((mod >>> (5 * (5 - i))) & 31);
  return out;
}

/** Bech32-encode raw bytes under `hrp` (no length cap — Cardano CIP-5 addresses). */
export function bech32Encode(hrp: string, bytes: Uint8Array): string {
  // 8-bit → 5-bit groups (pad the final group).
  const data5: number[] = [];
  let acc = 0;
  let bits = 0;
  for (const b of bytes) {
    acc = (acc << 8) | b;
    bits += 8;
    while (bits >= 5) {
      bits -= 5;
      data5.push((acc >>> bits) & 31);
    }
  }
  if (bits > 0) data5.push((acc << (5 - bits)) & 31);
  const combined = [...data5, ...bech32Checksum(hrp, data5)];
  return hrp + '1' + combined.map((d) => BECH32_ALPHABET[d]).join('');
}

/**
 * The reward (stake1…) address of a base payment address, or null when it has no stake
 * part (enterprise / Byron) or isn't decodable. Rebuilds the reward address from the
 * payment address's network + stake credential (bytes 29-56) and its script/key type
 * (CIP-19 header nibble). Used to collapse many payment addresses of one account to a
 * single stake address for the folded stake-change summary.
 */
export function stakeAddressOf(addr: string): string | null {
  const bytes = bech32Decode(addr);
  if (!bytes || bytes.length < 57) return null;
  const header = bytes[0];
  const network = header & 0x0f;
  const stakeIsScript = ((header >> 4) & 0b0010) !== 0; // base-address type bit 1 = stake is script
  const rewardHeader = (((0b1110 | (stakeIsScript ? 1 : 0)) << 4) | network) & 0xff;
  const data = new Uint8Array(29);
  data[0] = rewardHeader;
  data.set(bytes.slice(29, 57), 1);
  return bech32Encode(network === 1 ? 'stake' : 'stake_test', data);
}
