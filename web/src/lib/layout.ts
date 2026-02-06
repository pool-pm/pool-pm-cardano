export const TX_WIDTH = 180;
export const TX_GAP = 6;

export function squareWidth(count: number): number {
	const cols = Math.max(1, Math.ceil(Math.sqrt(count)));
	return cols * TX_WIDTH + (cols - 1) * TX_GAP;
}

