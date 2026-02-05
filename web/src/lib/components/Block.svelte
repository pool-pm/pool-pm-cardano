<script lang="ts">
	import { flip } from 'svelte/animate';
	import type { FeedBlock } from '../types';
	import type { TransitionConfig } from 'svelte/transition';
	import Transaction from './Transaction.svelte';

	type CrossfadeFn = (node: Element, params: { key: string }) => () => TransitionConfig;

	let { block, send, receive }: { block: FeedBlock; send: CrossfadeFn; receive: CrossfadeFn } = $props();

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
	// Each tx card is ~180px + 6px gap, we want cols ≈ rows
	const TX_WIDTH = 186; // 180px card + 6px gap
	const idealCols = $derived(Math.max(1, Math.ceil(Math.sqrt(reversedTxs.length))));
	const idealWidth = $derived(idealCols * TX_WIDTH + 20); // +20 for padding
</script>

<div class="block-card" style="border-color: {color}; --ideal-width: {idealWidth}px">
	<div class="block-header">
		<span class="block-number" style="color: {color}">
			#{block.number}
		</span>
		<span class="block-slot mono">slot {block.slot}</span>
		<span class="block-time">{timeAgo(block.timestamp)}</span>
	</div>

	{#if reversedTxs.length > 0}
		<div class="block-txs">
			{#each reversedTxs as tx (tx.hash)}
				<div animate:flip={{ duration: 300 }} in:receive={{ key: tx.hash }} out:send={{ key: tx.hash }}>
					<Transaction {tx} />
				</div>
			{/each}
		</div>
	{:else}
		<div class="block-meta muted">
			{block.tx_hashes.length} transactions
		</div>
	{/if}
</div>

<style>
	.block-card {
		background: var(--surface);
		border: 2px solid;
		border-radius: 8px;
		padding: 10px;
		width: min(var(--ideal-width), 100%);
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

	.block-meta {
		font-size: 12px;
		text-align: center;
		flex: 1;
		display: flex;
		align-items: center;
		justify-content: center;
	}

	.block-txs {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		justify-content: center;
	}

	.muted {
		color: var(--text-muted);
	}
</style>
