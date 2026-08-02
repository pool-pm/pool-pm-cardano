/** Kept short enough to sit under a thumbnail at 108px. */
const FINGERPRINT_HEAD = 9;
const FINGERPRINT_TAIL = 4;

/** Derive a collection-style label from a policy's sample asset names by taking their
 * longest common prefix and dropping a trailing index/separator run — so
 * ["Clay Nation #4821", "Clay Nation #12"] → "Clay Nation". Returns '' when the names
 * share no meaningful prefix (the caller falls back to a count label). */
export function commonNamePrefix(names: (string | undefined)[]): string {
  const valid = names.filter((n): n is string => typeof n === 'string' && n.length > 0);
  if (valid.length === 0) return '';

  // Longest common prefix across all names.
  let prefix = valid[0];
  for (let k = 1; k < valid.length && prefix; k++) {
    const n = valid[k];
    let i = 0;
    const max = Math.min(prefix.length, n.length);
    while (i < max && prefix[i] === n[i]) i++;
    prefix = prefix.slice(0, i);
  }

  // Drop a trailing "numbering" run — separators and/or digits left dangling by the LCP
  // ("Clay Nation #48" → "Clay Nation", "SpaceBudz #" → "SpaceBudz").
  const trimmed = prefix.replace(/[\s#/:_.·-]*\d*[\s#/:_.·-]*$/u, '').trim();
  return trimmed;
}

/**
 * What to call an asset on a tile.
 *
 * An on-chain asset name is bytes, not text. Protocol state tokens are named `\x00` and
 * `\x01`, which render as nothing or as replacement characters — so a mint of one showed
 * an empty label beside a picture that doesn't exist either. The fingerprint is derived
 * from the policy and name together (CIP-14), so it always exists and always identifies
 * the asset, and it stands in whenever the name has nothing readable in it.
 */
export function assetLabel(asset: { name?: string; fingerprint: string }): string {
  const name = asset.name?.trim();
  if (name && /[\p{L}\p{N}]/u.test(name)) return name;
  // Middle-truncated: a fingerprint is 44 characters and the tile is 108px wide, and
  // both ends of it identify while the middle does not.
  const fp = asset.fingerprint;
  return fp.length > FINGERPRINT_HEAD + FINGERPRINT_TAIL
    ? fp.slice(0, FINGERPRINT_HEAD) + '…' + fp.slice(-FINGERPRINT_TAIL)
    : fp;
}
