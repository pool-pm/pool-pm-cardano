export const TX_WIDTH = 108;
export const TX_GAP = 6;
export const FLIP_DURATION = 300;
export const SECTION_GAP = 12;

/** Grid content width: use as many columns as fit maxWidth, shrink-wrap around actual count. */
export function gridWidth(count: number, maxWidth: number): number {
  const maxCols = Math.max(1, Math.floor((maxWidth + TX_GAP) / (TX_WIDTH + TX_GAP)));
  const cols = Math.min(Math.max(1, count), maxCols);
  return cols * TX_WIDTH + Math.max(0, cols - 1) * TX_GAP;
}
