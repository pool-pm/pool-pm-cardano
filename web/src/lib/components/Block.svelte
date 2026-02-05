<script lang="ts">
	import type { FeedBlock } from '../types';
	import Transaction from './Transaction.svelte';

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
</script>

<div class="block-card" style="border-left-color: {color}">
	<div class="block-header">
		<span class="block-number" style="color: {color}">
			Block #{block.number}
		</span>
		<span class="block-slot mono">slot {block.slot}</span>
		<span class="block-time">{timeAgo(block.timestamp)}</span>
	</div>

	<div class="block-meta">
		<span>{block.tx_hashes.length} transactions</span>
	</div>

	{#if block.txs.length > 0}
		<div class="block-txs">
			{#each block.txs.slice(0, 5) as tx (tx.hash)}
				<Transaction {tx} />
			{/each}
			{#if block.txs.length > 5}
				<div class="more muted">
					+{block.txs.length - 5} more transactions
				</div>
			{/if}
		</div>
	{/if}
</div>

<style>
	.block-card {
		background: var(--surface);
		border: 1px solid var(--border);
		border-left: 4px solid;
		border-radius: 8px;
		padding: 12px 14px;
		margin-bottom: 8px;
	}

	.block-header {
		display: flex;
		align-items: center;
		gap: 12px;
		margin-bottom: 6px;
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

	.block-meta {
		color: var(--text-muted);
		font-size: 12px;
		margin-bottom: 8px;
	}

	.block-txs {
		border-top: 1px solid var(--border);
		padding-top: 8px;
	}

	.more {
		text-align: center;
		font-size: 12px;
		padding: 4px;
	}

	.muted {
		color: var(--text-muted);
	}
</style>
