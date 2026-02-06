<script lang="ts">
	import { flip } from 'svelte/animate';
	import { sections } from '../stores';
	import { TX_WIDTH, TX_GAP, FLIP_DURATION, squareWidth } from '../layout';
	import BinPackGrid from './BinPackGrid.svelte';
	import Transaction from './Transaction.svelte';
	import type { Section } from '../types';

	const MAX_AGE_MS = 600_000;
	const MAX_BLOCKS = 30;
	const PX_PER_SECOND = 2;
	const BLOCK_PADDING = 10;
	const BLOCK_BORDER = 2;
	const BLOCK_INSET = (BLOCK_PADDING + BLOCK_BORDER) * 2;

	let now = $state(Date.now());

	// Update current time every second for timeAgo display
	$effect(() => {
		const interval = setInterval(() => {
			now = Date.now();
		}, 1000);
		return () => clearInterval(interval);
	});

	// Clean up old sections periodically
	$effect(() => {
		const interval = setInterval(() => {
			const cutoff = Date.now() - MAX_AGE_MS;
			sections.update((s) => {
				let changed = false;

				// Clean old mempool txs (first section)
				const mempool = s[0];
				const before = mempool.txs.length;
				mempool.txs = mempool.txs.filter((tx) => tx.receivedAt >= cutoff);
				if (mempool.txs.length !== before) changed = true;

				// Remove old block sections
				const filtered = s.filter(
					(section, i) => i === 0 || section.receivedAt >= cutoff
				);
				if (filtered.length !== s.length) {
					s = filtered;
					changed = true;
				}

				// Enforce max block count (keep mempool + MAX_BLOCKS blocks)
				if (s.length > MAX_BLOCKS + 1) {
					s = s.slice(0, MAX_BLOCKS + 1);
					changed = true;
				}

				return changed ? [...s] : s;
			});
		}, 10_000);
		return () => clearInterval(interval);
	});

	function blockColor(hash: string): string {
		return '#' + hash.slice(0, 6);
	}

	function timeAgo(timestamp: number): string {
		const sec = Math.floor((now - timestamp * 1000) / 1000);
		if (sec < 60) return `${sec}s ago`;
		if (sec < 3600) return `${Math.floor(sec / 60)}m ago`;
		return `${Math.floor(sec / 3600)}h ago`;
	}
</script>

<div class="feed" style:--block-padding="{BLOCK_PADDING}px" style:--block-border="{BLOCK_BORDER}px">
	{#each $sections as section, i (section.id)}
		{@const color = section.block ? blockColor(section.block.hash) : undefined}
		{@const maxWidth = squareWidth(section.txs.length) + (section.block ? BLOCK_INSET : 0)}
		{@const prevTimestamp = i === 1
			? ($sections[0].txs[0]?.receivedAt ?? 0) / 1000
			: i > 1 ? $sections[i - 1].block?.timestamp : undefined}
		{@const gap = prevTimestamp && section.block
			? Math.max(0, (prevTimestamp - section.block.timestamp) * PX_PER_SECOND)
			: 0}
		<div
			class="section"
			class:block={!!section.block}
			style:border-color={color}
			style:max-width="{maxWidth}px"
			style:margin-top="{gap}px"
			animate:flip={{ duration: FLIP_DURATION }}
		>
			{#if section.block}
				<div class="block-header">
					<span class="block-number" style:color={color}>
						#{section.block.number}
					</span>
					<span class="block-slot mono">slot {section.block.slot}</span>
					<span class="block-time">{timeAgo(section.block.timestamp)}</span>
				</div>
			{/if}

			{#if section.txs.length > 0}
				<BinPackGrid items={section.txs} key={(tx) => tx.hash} itemWidth={TX_WIDTH} gap={TX_GAP}>
					{#snippet children(tx)}
						<Transaction {tx} />
					{/snippet}
				</BinPackGrid>
			{/if}
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
	}

	.section {
		width: 100%;
	}

	.section.block {
		background: var(--surface);
		border: var(--block-border) solid;
		border-radius: 8px;
		padding: var(--block-padding);
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
