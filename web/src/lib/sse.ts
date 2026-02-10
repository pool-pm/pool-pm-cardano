import { sections, newSection, config } from './stores';
import type { BlockEvent, Config, Event, MempoolTxEvent, Section } from './types';

let source: EventSource | null = null;

function handleSnapshot(events: Event[]): void {
	const now = Date.now();
	const blockEvents: BlockEvent[] = [];
	const mempoolTxEvents: MempoolTxEvent[] = [];

	for (const event of events) {
		if (event.type === 'Block') blockEvents.push(event);
		else if (event.type === 'MempoolTx') mempoolTxEvents.push(event);
	}

	// Build block sections (snapshot arrives oldest-first, sections are newest-first)
	const blockSections: Section[] = blockEvents.map((event) => {
		const section = newSection();
		section.block = {
			slot: event.slot,
			hash: event.hash,
			number: event.number,
			timestamp: event.timestamp,
			pool_id: event.pool_id,
			pool_ticker: event.pool_ticker,
		};
		section.txs = event.txs.map((tx) => ({ ...tx, receivedAt: now }));
		return section;
	}).reverse();

	// Build mempool
	const mempool = newSection();
	mempool.txs = mempoolTxEvents.map((tx) => ({ ...tx, receivedAt: now }));

	sections.set([mempool, ...blockSections]);
}

function handleEvent(event: Event): void {
	switch (event.type) {
		case 'MempoolTx': {
			const now = Date.now();
			sections.update((s) => {
				const mempool = s[0];
				if (!mempool.txs.some((t) => t.hash === event.hash)) {
					mempool.txs = [{ ...event, receivedAt: now }, ...mempool.txs];
				}
				// Resolve unresolved inputs from other mempool txs' outputs
				const outputsByHash = new Map(mempool.txs.map((tx) => [tx.hash, tx.outputs]));
				for (const tx of mempool.txs) {
					for (const input of tx.inputs) {
						if (!input.address) {
							const output = outputsByHash.get(input.tx_hash)?.[input.index];
							if (output) {
								input.address = output.address;
								input.lovelace = output.lovelace;
							}
						}
					}
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
				const mempoolByHash = new Map(mempool.txs.map((tx) => [tx.hash, tx]));

				const excluded = mempool.txs.filter((tx) => !blockTxHashes.has(tx.hash));

				const inBlock = event.txs.map((tx) => ({
					...tx,
					receivedAt: mempoolByHash.get(tx.hash)?.receivedAt ?? now,
				}));

				// Finalize current mempool as a block
				mempool.block = {
					slot: event.slot,
					hash: event.hash,
					number: event.number,
					timestamp: event.timestamp,
					pool_id: event.pool_id,
					pool_ticker: event.pool_ticker,
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
}

export function connectSSE(url: string): void {
	if (source) source.close();

	source = new EventSource(url);

	source.onmessage = (e: MessageEvent) => {
		const data = JSON.parse(e.data);

		if (data.type === 'Config') {
			config.set(data as Config);
		} else if (Array.isArray(data)) {
			handleSnapshot(data as Event[]);
		} else {
			handleEvent(data as Event);
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
