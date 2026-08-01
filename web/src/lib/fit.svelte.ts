/**
 * Fit text to a width by measuring it, rather than guessing from its length.
 *
 * A tx tile is 108px wide and the amount is the line a reader should land on first, so
 * it wants to be as large as the tile allows — which depends on the actual glyphs, not
 * the character count ("111" and "888" are the same length and not the same width).
 * `@chenglou/pretext` measures against the browser's own font engine off the DOM, so
 * this costs no layout and no reflow.
 *
 * Width scales linearly with font size for a given face, so one measurement at a
 * reference size gives the largest size that fits by division — no search, no loop.
 */
import { clearCache, measureNaturalWidth, prepareWithSegments } from '@chenglou/pretext';

/** Big enough that rounding in the measurement is far below one rendered pixel. */
const REFERENCE_PX = 100;

/**
 * Text measured before the webfont arrives is measured against the fallback face and
 * comes out at the wrong width. Reading this inside `fitFontSize` makes every fitted
 * size re-derive once the real face is in.
 */
let fontsReady = $state(false);

if (typeof document !== 'undefined' && document.fonts) {
  document.fonts.ready.then(() => {
    clearCache(); // drop everything measured against the fallback
    fontsReady = true;
  });
}

/**
 * Whether the webfont has landed. Read it from an effect that has to run again once
 * every fitted size on the page changes at once.
 */
export function fontsAreReady(): boolean {
  return fontsReady;
}

export interface FitBounds {
  /** Never go below this, even if the text still overflows — it would stop being legible. */
  min: number;
  /** Never go above this, however short the text is. */
  max: number;
}

/**
 * The largest font size, within `bounds`, at which `text` fits `available` pixels on one
 * line in `family` at `weight`.
 */
export function fitFontSize(
  text: string,
  family: string,
  weight: number,
  available: number,
  bounds: FitBounds,
): number {
  void fontsReady; // dependency, so a fallback-face measurement doesn't stick
  if (!text) return bounds.max;
  const natural = measureNaturalWidth(prepareWithSegments(text, `${weight} ${REFERENCE_PX}px ${family}`));
  if (!(natural > 0)) return bounds.max;
  return Math.max(bounds.min, Math.min(bounds.max, Math.floor((available * REFERENCE_PX) / natural)));
}
