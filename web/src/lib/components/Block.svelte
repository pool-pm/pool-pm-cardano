<script lang="ts">
	import type { FeedBlock } from '../types';
	import { TX_WIDTH, TX_GAP, squareWidth } from '../layout';
	import Transaction from './Transaction.svelte';
	import BinPackGrid from './BinPackGrid.svelte';

	let { block }: { block: FeedBlock } = $props();

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

	// Compute ideal width for a square-ish layout (+20 for card padding)
	const idealWidth = $derived(squareWidth(block.txs.length) + 20);
</script>

<div class="block-card" style="border-color: {color}; max-width: {idealWidth}px">
	<div class="block-header">
		<span class="block-number" style="color: {color}">
			#{block.number}
		</span>
		<span class="block-slot mono">slot {block.slot}</span>
		<span class="block-time">{timeAgo(block.timestamp)}</span>
	</div>

	{#if block.txs.length > 0}
		<BinPackGrid items={block.txs} key={(tx) => tx.hash} itemWidth={TX_WIDTH} gap={TX_GAP} crossAnimate>
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
