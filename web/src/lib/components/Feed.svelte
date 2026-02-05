<script lang="ts">
	import { flip } from 'svelte/animate';
	import { mempoolTxs, blocks } from '../stores';
	import Block from './Block.svelte';
	import BinPackGrid from './BinPackGrid.svelte';
	import Transaction from './Transaction.svelte';
	import type { FeedTx, FeedBlock } from '../types';

	const MAX_AGE_MS = 600_000;
	const MAX_BLOCKS = 30;

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
				// Enforce max block count
				if (map.size > MAX_BLOCKS) {
					const sorted = [...map.entries()].sort((a, b) => b[1].receivedAt - a[1].receivedAt);
					map = new Map(sorted.slice(0, MAX_BLOCKS));
					changed = true;
				}
				return changed ? new Map(map) : map;
			});
		}, 10_000);
		return () => clearInterval(interval);
	});

	// Mempool txs sorted newest first
	let sortedTxs: FeedTx[] = $derived.by(() => {
		const txs = [...$mempoolTxs.values()];
		txs.sort((a, b) => b.receivedAt - a.receivedAt);
		return txs;
	});

	// Blocks sorted newest first
	let sortedBlocks: FeedBlock[] = $derived.by(() => {
		const blks = [...$blocks.values()];
		blks.sort((a, b) => b.receivedAt - a.receivedAt);
		return blks;
	});

</script>

<div class="feed">
	{#if sortedTxs.length > 0}
		<div class="mempool-section">
			<BinPackGrid items={sortedTxs} key={(tx) => tx.hash} itemWidth={180} gap={8}>
				{#snippet children(tx)}
					<Transaction {tx} />
				{/snippet}
			</BinPackGrid>
		</div>
	{/if}
	{#each sortedBlocks as block (block.hash)}
		<div animate:flip={{ duration: 300 }}>
			<Block {block} />
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

	.mempool-section {
		width: 100%;
	}
</style>
