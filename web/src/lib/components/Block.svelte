<script lang="ts">
	import type { FeedBlock } from '../types';
	import Transaction from './Transaction.svelte';
	import BinPackGrid from './BinPackGrid.svelte';

	let { block }: { block: FeedBlock } = $props();

	const TX_WIDTH = 180;
	const GAP = 6;

	function blockColor(hash: string): string {
		return '#' + hash.slice(0, 6);
	}

	function timeAgo(timestamp: number): string {
		const sec = Math.floor((Date.now() - timestamp * 1000) / 1000);
		if (sec < 60) return `${sec}s ago`;
		if (sec < 3600) return `${Math.floor(sec / 60)}m ago`;
		return `${Math.floor(sec / 3600)}h ago`;
	}

	const color = $derived(blockColor(block.hash));

	// Reverse order: first tx in block appears last visually
	const reversedTxs = $derived([...block.txs].reverse());

	// Compute ideal width for a square-ish layout
	const idealCols = $derived(Math.max(1, Math.ceil(Math.sqrt(reversedTxs.length))));
	const idealWidth = $derived(idealCols * TX_WIDTH + (idealCols - 1) * GAP + 20); // +20 for padding
</script>

<div class="block-card" style="border-color: {color}; max-width: {idealWidth}px">
	<div class="block-header">
		<span class="block-number" style="color: {color}">
			#{block.number}
		</span>
		<span class="block-slot mono">slot {block.slot}</span>
		<span class="block-time">{timeAgo(block.timestamp)}</span>
	</div>

	{#if reversedTxs.length > 0}
		<BinPackGrid items={reversedTxs} key={(tx) => tx.hash} itemWidth={TX_WIDTH} gap={GAP}>
			{#snippet children(tx)}
				<Transaction {tx} />
			{/snippet}
		</BinPackGrid>
	{/if}
</div>

<style>
	.block-card {
		background: var(--surface);
		border: 2px solid;
		border-radius: 8px;
		padding: 10px;
		width: 100%;
		margin: 0 auto;
		display: flex;
		flex-direction: column;
	}

	.block-header {
		display: flex;
		align-items: center;
		gap: 8px;
		margin-bottom: 8px;
		flex-shrink: 0;
	}

	.block-number {
		font-weight: 700;
		font-size: 14px;
	}

	.block-slot {
		color: var(--text-muted);
		font-size: 11px;
	}

	.block-time {
		color: var(--text-muted);
		font-size: 11px;
		margin-left: auto;
	}

</style>
