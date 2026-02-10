export const TX_WIDTH = 108;
export const TX_GAP = 6;
export const FLIP_DURATION = 300;

export function squareWidth(count: number): number {
	const cols = Math.max(1, Math.ceil(Math.sqrt(count)));
	return cols * TX_WIDTH + (cols - 1) * TX_GAP;
}

