import type { TxInput, TxOutputInfo } from './types';
import { bech32Decode } from './bech32';

// --- Quantity arithmetic ---

/** Scaled bigint: [integer_value, decimal_places]. */
export type ScaledQty = [bigint, number];

/** Parse a formatted quantity string (e.g. "291922.894186") to a scaled bigint. */
export function parseQuantity(s: string): ScaledQty {
  const dot = s.indexOf('.');
  if (dot === -1) return [BigInt(s), 0];
  const decimals = s.length - dot - 1;
  return [BigInt(s.slice(0, dot) + s.slice(dot + 1)), decimals];
}

/** Add two scaled quantities, aligning decimals. */
export function addQuantities(a: ScaledQty, b: ScaledQty): ScaledQty {
  const scale = Math.max(a[1], b[1]);
  const va = a[0] * 10n ** BigInt(scale - a[1]);
  const vb = b[0] * 10n ** BigInt(scale - b[1]);
  return [va + vb, scale];
}

/** Compare two scaled quantities. Returns true if a > b. */
function gtQuantity(a: ScaledQty, b: ScaledQty): boolean {
  const scale = Math.max(a[1], b[1]);
  return a[0] * 10n ** BigInt(scale - a[1]) > b[0] * 10n ** BigInt(scale - b[1]);
}

// --- Credential grouping ---

function bytesToHex(bytes: Uint8Array, start: number, end: number): string {
  return Array.from(bytes.slice(start, end))
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('');
}

export interface CredGroup {
  assets: Map<string, ScaledQty>;
  inputLovelace: bigint;
}

interface StakeCredGroup extends CredGroup {
  header: number;
}

export interface InputCreds {
  byAddress: Map<string, CredGroup>;
  byPayCred: Map<string, CredGroup>;
  byStakeCred: Map<string, StakeCredGroup>;
}

function addToGroup<T extends CredGroup>(map: Map<string, T>, key: string, init: () => T, input: TxInput): void {
  let group = map.get(key);
  if (!group) {
    group = init();
    map.set(key, group);
  }
  group.inputLovelace += BigInt(input.lovelace);
  for (const a of input.assets ?? []) {
    const qty = parseQuantity(a.quantity);
    const prev = group.assets.get(a.fingerprint);
    group.assets.set(a.fingerprint, prev ? addQuantities(prev, qty) : qty);
  }
}

export function buildInputCreds(inputs: TxInput[]): InputCreds {
  const byAddress = new Map<string, CredGroup>();
  const byPayCred = new Map<string, CredGroup>();
  const byStakeCred = new Map<string, StakeCredGroup>();

  for (const input of inputs) {
    if (!input.address) continue;
    addToGroup(byAddress, input.address, () => ({ assets: new Map(), inputLovelace: 0n }), input);

    const bytes = bech32Decode(input.address);
    if (!bytes || bytes.length < 29) continue;

    addToGroup(byPayCred, bytesToHex(bytes, 1, 29), () => ({ assets: new Map(), inputLovelace: 0n }), input);

    if (bytes.length >= 57) {
      const scriptBit = (bytes[0] >> 4) & 1;
      addToGroup(
        byStakeCred,
        scriptBit + bytesToHex(bytes, 29, 57),
        () => ({ assets: new Map(), inputLovelace: 0n, header: bytes[0] }),
        input,
      );
    }
  }
  return { byAddress, byPayCred, byStakeCred };
}

export function matchGroup(output: TxOutputInfo, creds: InputCreds): CredGroup | undefined {
  const addrGroup = creds.byAddress.get(output.address);
  if (addrGroup) return addrGroup;

  const bytes = bech32Decode(output.address);
  if (!bytes || bytes.length < 29) return undefined;

  const payGroup = creds.byPayCred.get(bytesToHex(bytes, 1, 29));
  if (payGroup) return payGroup;

  if (bytes.length >= 57) {
    const scriptBit = (bytes[0] >> 4) & 1;
    const info = creds.byStakeCred.get(scriptBit + bytesToHex(bytes, 29, 57));
    if (info && bytes[0] >> 4 === info.header >> 4) return info;
  }
  return undefined;
}

/** Strip change assets from an output, keeping only assets that are new or
 *  whose quantity exceeds what the input group had. */
function stripChangeAssets(output: TxOutputInfo, group: CredGroup): TxOutputInfo {
  const filtered = output.assets.filter((a) => {
    const inQty = group.assets.get(a.fingerprint);
    if (!inQty) return true; // new asset, keep
    return gtQuantity(parseQuantity(a.quantity), inQty); // keep if output qty > input qty
  });
  return filtered.length === output.assets.length ? output : { ...output, assets: filtered };
}

/** Compute non-change outputs for a transaction. */
export function nonChangeOutputs(inputs: TxInput[], outputs: TxOutputInfo[]): TxOutputInfo[] {
  const creds = buildInputCreds(inputs);

  interface OutputGroup {
    outputs: TxOutputInfo[];
    group: CredGroup;
    totalLovelace: bigint;
    totalAssets: Map<string, ScaledQty>;
  }
  const outputGroups = new Map<CredGroup, OutputGroup>();
  const unmatched: { output: TxOutputInfo; group: CredGroup | undefined }[] = [];

  for (const output of outputs) {
    const group = matchGroup(output, creds);
    if (!group) {
      unmatched.push({ output, group: undefined });
      continue;
    }

    // Output has an asset not present in matched inputs -> not change
    if (output.assets.some((a) => !group.assets.has(a.fingerprint))) {
      unmatched.push({ output, group });
      continue;
    }

    let entry = outputGroups.get(group);
    if (!entry) {
      entry = { outputs: [], group, totalLovelace: 0n, totalAssets: new Map() };
      outputGroups.set(group, entry);
    }
    entry.outputs.push(output);
    entry.totalLovelace += BigInt(output.lovelace);
    for (const a of output.assets) {
      const qty = parseQuantity(a.quantity);
      const prev = entry.totalAssets.get(a.fingerprint);
      entry.totalAssets.set(a.fingerprint, prev ? addQuantities(prev, qty) : qty);
    }
  }

  const result: TxOutputInfo[] = [];
  for (const { output, group } of unmatched) {
    result.push(group ? stripChangeAssets(output, group) : output);
  }
  for (const [group, { outputs: grpOutputs, totalLovelace, totalAssets }] of outputGroups) {
    let received = totalLovelace > group.inputLovelace;
    if (!received) {
      for (const [fp, outQty] of totalAssets) {
        const inQty = group.assets.get(fp);
        if (!inQty) {
          received = true;
          break;
        }
        if (gtQuantity(outQty, inQty)) {
          received = true;
          break;
        }
      }
    }
    if (received) {
      for (const o of grpOutputs) result.push(stripChangeAssets(o, group));
    }
  }
  return result;
}
