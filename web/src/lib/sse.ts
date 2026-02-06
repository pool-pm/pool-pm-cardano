import { mempoolTxs, blocks } from './stores';
import type { Event } from './types';

let source: EventSource | null = null;

export function connectSSE(url: string): void {
	if (source) source.close();

	source = new EventSource(url);

	source.onmessage = (e: MessageEvent) => {
		const event: Event = JSON.parse(e.data);

		switch (event.type) {
			case 'MempoolTx':
				mempoolTxs.update((map) => {
					map.set(event.hash, { ...event, receivedAt: Date.now() });
					return new Map(map);
				});
				break;

			case 'Block': {
				const now = Date.now();

				// Remove confirmed txs from mempool display
				mempoolTxs.update((map) => {
					for (const tx of event.txs) {
						map.delete(tx.hash);
					}
					return new Map(map);
				});

				blocks.update((map) => {
					map.set(event.hash, {
						...event,
						receivedAt: now,
						txs: event.txs.map((tx) => ({ ...tx, receivedAt: now })),
					});
					return new Map(map);
				});
				break;
			}

			case 'Rollback':
				blocks.update((map) => {
					const toDelete: string[] = [];
					for (const [hash, block] of map) {
						if (block.slot > event.slot) {
							toDelete.push(hash);
						}
					}
					if (toDelete.length > 0) {
						const newMap = new Map(map);
						for (const hash of toDelete) {
							newMap.delete(hash);
						}
						return newMap;
					}
					return map;
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
