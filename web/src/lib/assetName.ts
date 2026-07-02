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
