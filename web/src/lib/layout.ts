export const TX_WIDTH = 148;
export const TX_GAP = 6;
export const FLIP_DURATION = 300;

export function formatTicker(ticker: string): string {
  return ticker
    .toUpperCase()
    .replace(/[^A-Z0-9]/g, '')
    .slice(0, 5);
}

export function poolColor(poolId?: string): string {
  const key = poolId?.slice(5) ?? '';
  let h = 0;
  for (let i = 0; i < key.length; i++) {
    h = Math.imul(h ^ key.charCodeAt(i), 0x9e3779b9);
  }
  return `oklch(0.7 0.25 ${(h >>> 0) % 360})`;
}

// --- Unified grid layout action ---

export type LayoutGridParams = {
  landscape: boolean;
  availableWidth: number;
  availableHeight: number;
};

function colsForMaxH(heights: number[], gap: number, maxH: number): number {
  let cols = 1,
    h = 0;
  for (const itemH of heights) {
    if (h > 0 && h + gap + itemH > maxH) {
      cols++;
      h = itemH;
    } else {
      h += (h > 0 ? gap : 0) + itemH;
    }
  }
  return cols;
}

/** Portrait: bottom-up bin-packing, oldest at bottom-right, newest at top-left */
function layoutPortrait(
  items: HTMLElement[],
  heights: number[],
  gap: number,
  availableWidth: number,
  containerWidth: number,
): { gridWidth: number; gridHeight: number } {
  const colCount = Math.max(1, Math.floor((availableWidth + gap) / (TX_WIDTH + gap)));
  const colHeights = new Array(colCount).fill(0);
  const itemData: { idx: number; col: number; y: number; height: number }[] = [];
  let maxColUsed = -1;

  // Process from oldest (end of array) to newest (start)
  for (let i = items.length - 1; i >= 0; i--) {
    const height = heights[i];

    // Among used columns, find shortest (rightmost if tie)
    let shortest = 0;
    for (let c = 1; c <= maxColUsed; c++) {
      if (colHeights[c] <= colHeights[shortest]) shortest = c;
    }

    const currentMax = maxColUsed >= 0 ? Math.max(...colHeights.slice(0, maxColUsed + 1)) : 0;
    const canStack = maxColUsed >= 0 && colHeights[shortest] + height + gap <= currentMax;

    let targetCol: number;
    if (canStack) targetCol = shortest;
    else if (maxColUsed < colCount - 1) targetCol = maxColUsed + 1;
    else targetCol = shortest;

    maxColUsed = Math.max(maxColUsed, targetCol);

    const y = colHeights[targetCol];
    itemData.push({ idx: i, col: targetCol, y, height });
    colHeights[targetCol] = y + height + gap;
  }

  const totalHeight = Math.max(0, Math.max(...colHeights) - gap);
  const actualCols = maxColUsed + 1;
  const gridWidth = actualCols * TX_WIDTH + Math.max(0, actualCols - 1) * gap;
  // Center within the actual container width, not the available layout width
  const offsetX = Math.max(0, (containerWidth - gridWidth) / 2);

  for (const { idx, col, y, height } of itemData) {
    const displayX = offsetX + col * (TX_WIDTH + gap);
    const displayY = totalHeight - y - height;
    items[idx].style.transform = `translate(${displayX}px, ${displayY}px)`;
  }

  return { gridWidth, gridHeight: totalHeight };
}

/** Landscape: balanced sequential columns with binary-search min max-height */
function layoutLandscape(
  items: HTMLElement[],
  heights: number[],
  gap: number,
  availableHeight: number,
): { gridWidth: number; gridHeight: number } {
  const total = heights.reduce((s, h) => s + h, 0) + Math.max(0, items.length - 1) * gap;
  const maxItem = Math.max(...heights);

  // Find minimum columns where the max column height fits availableHeight.
  // Start with the theoretical minimum, then increase if greedy packing overshoots.
  let numCols = total <= availableHeight ? 1 : Math.ceil(total / availableHeight);
  let maxH: number;
  while (numCols <= items.length) {
    let lo = maxItem;
    let hi = total;
    while (hi - lo > 1) {
      const mid = Math.floor((lo + hi) / 2);
      if (colsForMaxH(heights, gap, mid) <= numCols) hi = mid;
      else lo = mid;
    }
    maxH = hi;
    if (maxH <= availableHeight || numCols >= items.length) break;
    numCols++;
  }
  maxH = maxH!;

  // Greedy column assignment
  const cols: { idx: number; h: number }[][] = [[]];
  let colH = 0;
  for (let i = 0; i < items.length; i++) {
    if (colH > 0 && colH + gap + heights[i] > maxH) {
      cols.push([]);
      colH = 0;
    }
    cols[cols.length - 1].push({ idx: i, h: heights[i] });
    colH += (colH > 0 ? gap : 0) + heights[i];
  }

  // Compute grid dimensions
  const gridWidth = cols.length * TX_WIDTH + Math.max(0, cols.length - 1) * gap;
  let gridHeight = 0;
  for (const col of cols) {
    const ch = col.reduce((s, it) => s + it.h, 0) + Math.max(0, col.length - 1) * gap;
    gridHeight = Math.max(gridHeight, ch);
  }

  // Position items, bottom-aligned per column
  for (let ci = 0; ci < cols.length; ci++) {
    const col = cols[ci];
    const colTotal = col.reduce((s, it) => s + it.h, 0) + Math.max(0, col.length - 1) * gap;
    const x = ci * (TX_WIDTH + gap);
    let y = gridHeight - colTotal;
    for (const { idx, h } of col) {
      items[idx].style.transform = `translate(${x}px, ${y}px)`;
      y += h + gap;
    }
  }

  return { gridWidth, gridHeight };
}

/**
 * Svelte action that positions children in a grid using absolute positioning.
 * Portrait: bottom-up bin-packing (shortest column first).
 * Landscape: balanced sequential columns (binary-search optimal height).
 */
export function layoutGrid(node: HTMLElement, params: LayoutGridParams) {
  let { landscape, availableWidth, availableHeight } = params;
  let pendingFrame = 0;

  function doLayout() {
    pendingFrame = 0;
    const items = Array.from(node.children) as HTMLElement[];
    if (items.length === 0) {
      node.style.width = '';
      node.style.height = '';
      return;
    }

    const heights = items.map((el) => el.offsetHeight);
    const gap = TX_GAP;

    let gridWidth: number, gridHeight: number;
    if (landscape) {
      ({ gridWidth, gridHeight } = layoutLandscape(items, heights, gap, availableHeight));
      node.style.width = `${gridWidth}px`;
    } else {
      const w = availableWidth || node.offsetWidth;
      ({ gridWidth, gridHeight } = layoutPortrait(items, heights, gap, w, node.offsetWidth));
      node.style.width = '';
      node.dispatchEvent(new CustomEvent('gridwidth', { detail: gridWidth, bubbles: true }));
    }
    const maxHeight = landscape ? availableHeight : Infinity;
    node.style.height = `${Math.min(gridHeight, maxHeight)}px`;
  }

  function schedule() {
    if (!pendingFrame) {
      pendingFrame = requestAnimationFrame(doLayout);
    }
  }

  const mutObs = new MutationObserver(schedule);
  mutObs.observe(node, { childList: true });

  node.addEventListener('remeasure', schedule);

  const resizeObs = new ResizeObserver(schedule);
  resizeObs.observe(node);

  schedule();

  return {
    update(newParams: LayoutGridParams) {
      landscape = newParams.landscape;
      availableWidth = newParams.availableWidth;
      availableHeight = newParams.availableHeight;
      doLayout();
    },
    destroy() {
      if (pendingFrame) cancelAnimationFrame(pendingFrame);
      mutObs.disconnect();
      resizeObs.disconnect();
      node.removeEventListener('remeasure', schedule);
    },
  };
}
