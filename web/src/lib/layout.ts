export const TX_WIDTH = 108;
export const TX_GAP = 6;
export const FLIP_DURATION = 300;

export function poolColor(poolId?: string): string {
  const key = poolId?.slice(5) ?? '';
  let h = 0;
  for (let i = 0; i < key.length; i++) {
    h = Math.imul(h ^ key.charCodeAt(i), 0x9e3779b9);
  }
  return `oklch(0.7 0.25 ${(h >>> 0) % 360})`;
}
