export const TX_WIDTH = 108;
export const TX_GAP = 6;
export const FLIP_DURATION = 300;

export function formatTicker(ticker: string): string {
  return ticker
    .toUpperCase()
    .replace(/[^A-Z0-9]/g, '')
    .slice(0, 5);
}

/** Compact count for tight rows (e.g. search results): 1234 → "1.2k", 12345 → "12k". */
export function formatCount(n: number): string {
  if (n < 1000) return String(n);
  if (n < 10_000) return (n / 1000).toFixed(1).replace(/\.0$/, '') + 'k';
  if (n < 1_000_000) return Math.round(n / 1000) + 'k';
  if (n < 10_000_000) return (n / 1_000_000).toFixed(1).replace(/\.0$/, '') + 'M';
  if (n < 1_000_000_000) return Math.round(n / 1_000_000) + 'M';
  return (n / 1_000_000_000).toFixed(1).replace(/\.0$/, '') + 'B';
}

/**
 * A DRep's governance participation: `148 votes (98%)`.
 *
 * `votes` counts the distinct actions the DRep voted on (a re-vote on the same action counts
 * once) and `eligible` the actions it could have voted on — those whose voting window overlapped
 * its registration. The `%` is dropped when that denominator is unknown (0: an older server, or a
 * predefined DRep) and when the DRep hasn't voted at all, where a bare `0 votes` reads better
 * than `0 votes (0%)`. Clamped to 100 because `eligible` only refreshes at epoch boundaries, so a
 * vote on an action proposed mid-epoch can briefly outrun it.
 */
export function formatVotes(votes: number, eligible?: number): string {
  const n = `${votes.toLocaleString()} vote${votes === 1 ? '' : 's'}`;
  if (!eligible || votes === 0) return n;
  return `${n} (${Math.min(100, Math.round((100 * votes) / eligible))}%)`;
}

/** Compact ADA from a lovelace string for tight rows: "12345678000000" → "12.3M ₳". */
export function formatAdaCompact(lovelace: string): string {
  return formatCount(Number(BigInt(lovelace) / 1_000_000n)) + ' ₳';
}

/** Full ADA from a lovelace string (string slicing, no float arithmetic): inserts the
 * decimal point and groups the whole part for large values. The symbol is preceded by a
 * NARROW NO-BREAK SPACE (U+202F): narrow like a thin space, but a line can never wrap
 * between the amount and its ₳ — which it did, on a phone-width header. */
export function formatAda(lovelace: string): string {
  const padded = lovelace.padStart(7, '0');
  const whole = padded.slice(0, -6) || '0';
  const frac = padded.slice(-6);
  const wholeNum = Number(whole);
  if (wholeNum >= 1000) return wholeNum.toLocaleString() + ' ₳';
  if (wholeNum >= 1) {
    const trimmed = frac.slice(0, 2).replace(/0+$/, '');
    return trimmed ? whole + '.' + trimmed + ' ₳' : whole + ' ₳';
  }
  const trimmed = frac.replace(/0+$/, '');
  return trimmed ? '0.' + trimmed + ' ₳' : '0' + ' ₳';
}

/** Group the integer part of a decimals-formatted quantity string with locale thousands
 * separators, preserving any fractional part. Uses BigInt so it stays exact past
 * Number.MAX_SAFE_INTEGER (token supplies can be huge): "520000" → "520,000",
 * "1204.55" → "1,204.55", "0.000001" → "0.000001". */
export function formatQuantity(qty: string): string {
  if (!/[0-9]/.test(qty)) return qty; // empty / non-numeric: leave untouched
  const neg = qty.startsWith('-');
  const body = neg ? qty.slice(1) : qty;
  const dot = body.indexOf('.');
  const intPart = dot === -1 ? body : body.slice(0, dot);
  const frac = dot === -1 ? '' : body.slice(dot); // includes the leading '.'
  let grouped: string;
  try {
    grouped = new Intl.NumberFormat().format(BigInt(intPart || '0'));
  } catch {
    return qty; // non-numeric input: leave untouched
  }
  return (neg ? '-' : '') + grouped + frac;
}

// Largest sRGB-displayable OKLCH chroma for a given lightness & hue, found by
// binary-searching the oklab→linear-sRGB gamut boundary (Ottosson's matrices).
// Lets us vary chroma without straying past the gamut, where higher values just
// clamp to the same rendered color.
function maxSrgbChroma(l: number, hueDeg: number): number {
  const hr = (hueDeg * Math.PI) / 180;
  const ca = Math.cos(hr);
  const cb = Math.sin(hr);
  const inGamut = (c: number): boolean => {
    const a = c * ca;
    const b = c * cb;
    const l_ = l + 0.3963377774 * a + 0.2158037573 * b;
    const m_ = l - 0.1055613458 * a - 0.0638541728 * b;
    const s_ = l - 0.0894841775 * a - 1.291485548 * b;
    const lc = l_ * l_ * l_;
    const mc = m_ * m_ * m_;
    const sc = s_ * s_ * s_;
    const r = 4.0767416621 * lc - 3.3077115913 * mc + 0.2309699292 * sc;
    const g = -1.2684380046 * lc + 2.6097574011 * mc - 0.3413193965 * sc;
    const bl = -0.0041960863 * lc - 0.7034186147 * mc + 1.707614701 * sc;
    const eps = 1e-4;
    return r >= -eps && r <= 1 + eps && g >= -eps && g <= 1 + eps && bl >= -eps && bl <= 1 + eps;
  };
  let lo = 0;
  let hi = 0.4;
  for (let i = 0; i < 20; i++) {
    const mid = (lo + hi) / 2;
    if (inGamut(mid)) lo = mid;
    else hi = mid;
  }
  return lo;
}

const LIGHTNESS = 0.7; // fixed for legibility on the black background
// Fractions of each hue's max in-gamut chroma. Kept high so every color stays
// vivid; only 3 steps because the vivid band is narrow and chroma is ~perceptually
// uniform, so finer steps wouldn't be distinguishable (hue carries the variety).
const CHROMA_STEPS = [0.7, 0.85, 1];

export function poolColor(poolId?: string): string {
  const key = poolId?.slice(5) ?? '';
  let h = 0;
  for (let i = 0; i < key.length; i++) {
    h = Math.imul(h ^ key.charCodeAt(i), 0x9e3779b9);
  }
  const u = h >>> 0;
  const hue = u % 360;
  // Vary hue and chroma (not lightness) for more distinguishable colors. Chroma
  // is a fraction of this hue's gamut max, so it never clamps to a duplicate.
  const chroma = maxSrgbChroma(LIGHTNESS, hue) * CHROMA_STEPS[Math.floor(u / 360) % CHROMA_STEPS.length];
  return `oklch(${LIGHTNESS} ${chroma.toFixed(4)} ${hue})`;
}

// --- Unified grid layout action ---

export type LayoutGridParams = {
  landscape: boolean;
  availableWidth: number;
  availableHeight: number;
  /** Included only so the action's `update` re-runs the layout when a block folds/unfolds
   * (the tiles change height but none of the other params do). Not otherwise used. */
  foldRev?: unknown;
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

  // Pack tiles from the grid's left edge (x = 0). Centering is NOT done here: the grid is set
  // to exactly gridWidth and centered within the section by CSS (margin-inline: auto), so a
  // header wider than one tile column can enlarge the section without pushing the tiles
  // off-centre (the old JS offset centred within the measured node width, which was stale/wrong
  // whenever the header — not the grid — determined the section width).
  for (const { idx, col, y, height } of itemData) {
    const displayX = col * (TX_WIDTH + gap);
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
      ({ gridWidth, gridHeight } = layoutPortrait(items, heights, gap, w));
      // Size the grid to exactly its packed width; CSS margin-inline:auto then centres it in the
      // section, so the tiles stay centred even when a wide header widens the section.
      node.style.width = `${gridWidth}px`;
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
      // rAF so a fold/unfold re-measures the tiles *after* they've re-rendered at their new size.
      schedule();
    },
    destroy() {
      if (pendingFrame) cancelAnimationFrame(pendingFrame);
      mutObs.disconnect();
      resizeObs.disconnect();
      node.removeEventListener('remeasure', schedule);
    },
  };
}
