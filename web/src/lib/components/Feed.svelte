<script lang="ts">
	import { mempoolTxs, blocks } from '../stores';
	import Transaction from './Transaction.svelte';
	import Block from './Block.svelte';
	import type { FeedTx, FeedBlock } from '../types';

	const MAX_AGE_MS = 600_000;
	const GROUP_WINDOW_MS = 1000;

	// Clean up old items periodically
	$effect(() => {
		const interval = setInterval(() => {
			const cutoff = Date.now() - MAX_AGE_MS;
			mempoolTxs.update((map) => {
				let changed = false;
				for (const [hash, tx] of map) {
					if (tx.receivedAt < cutoff) {
						map.delete(hash);
						changed = true;
					}
				}
				return changed ? new Map(map) : map;
			});
			blocks.update((map) => {
				let changed = false;
				for (const [hash, block] of map) {
					if (block.receivedAt < cutoff) {
						map.delete(hash);
						changed = true;
					}
				}
				return changed ? new Map(map) : map;
			});
		}, 10_000);
		return () => clearInterval(interval);
	});

	type FeedItem =
		| { kind: 'tx'; key: string; receivedAt: number; data: FeedTx }
		| { kind: 'block'; key: string; receivedAt: number; data: FeedBlock };

	type FeedRow = { ts: number; items: FeedItem[] };

	let rows: FeedRow[] = $derived.by(() => {
		const txItems: FeedItem[] = [];
		const blockItems: FeedItem[] = [];

		for (const [hash, tx] of $mempoolTxs) {
			txItems.push({
				kind: 'tx',
				key: `tx-${hash}`,
				receivedAt: tx.receivedAt,
				data: tx,
			});
		}

		for (const [hash, block] of $blocks) {
			blockItems.push({
				kind: 'block',
				key: `blk-${hash}`,
				receivedAt: block.receivedAt,
				data: block,
			});
		}

		// newest first
		txItems.sort((a, b) => b.receivedAt - a.receivedAt);
		blockItems.sort((a, b) => b.receivedAt - a.receivedAt);

		// group mempool txs within the same second into rows
		const txRows: FeedRow[] = [];
		for (const item of txItems) {
			const last = txRows[txRows.length - 1];
			if (last && Math.abs(item.receivedAt - last.ts) < GROUP_WINDOW_MS) {
				last.items.push(item);
			} else {
				txRows.push({ ts: item.receivedAt, items: [item] });
			}
		}

		// blocks always get their own row
		const blockRows: FeedRow[] = blockItems.map((item) => ({
			ts: item.receivedAt,
			items: [item],
		}));

		// mempool txs first, then blocks
		return [...txRows, ...blockRows];
	});
</script>

<div class="feed">
	{#each rows as row (row.ts)}
		<div class="feed-row">
			{#each row.items as item (item.key)}
				{#if item.kind === 'tx'}
					<Transaction tx={item.data} />
				{:else}
					<Block block={item.data} />
				{/if}
			{/each}
		</div>
	{/each}
</div>

<style>
	.feed {
		flex: 1;
		overflow-y: auto;
		padding: 16px 20px;
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 8px;
	}

	.feed-row {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		justify-content: center;
	}
</style>
