import { sections, newSection } from './stores';
import type { Event } from './types';

let source: EventSource | null = null;

export function connectSSE(url: string): void {
	if (source) source.close();

	source = new EventSource(url);

	source.onmessage = (e: MessageEvent) => {
		const event: Event = JSON.parse(e.data);

		switch (event.type) {
			case 'MempoolTx': {
				const now = Date.now();
				sections.update((s) => {
					const mempool = s[0];
					if (!mempool.txs.some((t) => t.hash === event.hash)) {
						mempool.txs = [{ ...event, receivedAt: now }, ...mempool.txs];
					}
					return [...s];
				});
				break;
			}

			case 'Block': {
				const now = Date.now();
				sections.update((s) => {
					const mempool = s[0];
					const blockTxHashes = new Set(event.txs.map((tx) => tx.hash));

					// Split mempool txs: those in the block vs excluded
					const inBlock = [];
					const excluded = [];
					const seen = new Set<string>();

					for (const tx of mempool.txs) {
						if (blockTxHashes.has(tx.hash)) {
							inBlock.push(tx);
							seen.add(tx.hash);
						} else {
							excluded.push(tx);
						}
					}

					// Add block txs not previously in mempool
					for (const tx of event.txs) {
						if (!seen.has(tx.hash)) {
							inBlock.push({ ...tx, receivedAt: now });
						}
					}

					// Finalize current mempool as a block
					mempool.block = {
						slot: event.slot,
						hash: event.hash,
						number: event.number,
						timestamp: event.timestamp,
					};
					mempool.txs = inBlock;

					// New mempool with excluded txs
					const next = newSection();
					next.txs = excluded;

					return [next, ...s];
				});
				break;
			}

			case 'Rollback':
				sections.update((s) => {
					return s.filter(
						(section, i) =>
							i === 0 || !section.block || section.block.slot <= event.slot
					);
				});
				break;
		}
	};

	source.onerror = () => {
		source?.close();
		source = null;
		setTimeout(() => connectSSE(url), 3000);
	};
}

export function disconnectSSE(): void {
	source?.close();
	source = null;
}
