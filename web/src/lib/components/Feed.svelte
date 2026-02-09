<script lang="ts">
	import { flip } from 'svelte/animate';
	import { slide } from 'svelte/transition';
	import { sections } from '../stores';
	import { TX_WIDTH, TX_GAP, FLIP_DURATION, squareWidth } from '../layout';
	import BinPackGrid from './BinPackGrid.svelte';
	import Transaction from './Transaction.svelte';

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
				s[0].txs = s[0].txs.filter((tx) => tx.receivedAt >= cutoff);
				return s
					.filter((section, i) => i === 0 || section.receivedAt >= cutoff)
					.slice(0, MAX_BLOCKS + 1);
			});
		}, 10_000);
		return () => clearInterval(interval);
	});

	function blockColor(hash: string): string {
		const hue = (parseInt(hash.slice(0, 4), 16) / 0xffff) * 360;
		return `oklch(0.7 0.25 ${hue.toFixed(1)})`;
	}

	function timeAgo(timestamp: number): string {
		const sec = Math.floor((now - timestamp * 1000) / 1000);
		if (sec < 60) return `${sec}s ago`;
		if (sec < 3600) return `${Math.floor(sec / 60)}m ago`;
		return `${Math.floor(sec / 3600)}h ago`;
	}

	function formatTime(timestamp: number): string {
		return new Date(timestamp * 1000).toLocaleTimeString();
	}
</script>

<div class="feed" style:--block-padding="{BLOCK_PADDING}px" style:--block-border="{BLOCK_BORDER}px" style:--flip-duration="{FLIP_DURATION}ms">
	{#each $sections as section, i (section.id)}
		{@const isMempool = !section.block}
		{@const color = section.block ? blockColor(section.block.hash) : '#111'}
		{@const maxWidth = squareWidth(section.txs.length) + BLOCK_INSET}
		{@const prevTimestamp = i > 0
			? $sections[i - 1].block?.timestamp ?? ($sections[i - 1].txs[0]?.receivedAt ?? 0) / 1000
			: undefined}
		{@const gap = prevTimestamp && section.block
			? Math.max(0, (prevTimestamp - section.block.timestamp) * PX_PER_SECOND)
			: 0}
		{@const spacing = i > 0 ? Math.max(12, gap) : 0}
		<div
			class="section"
			class:mempool={isMempool}
			class:has-line={i > 0 && gap > 0}
			style:border-color={color}
			style:background-color={color}
			style:max-width="{maxWidth}px"
			style:margin-top="{spacing}px"
			style:--line-height="{gap}px"
			animate:flip={{ duration: FLIP_DURATION }}
			out:slide={{ duration: FLIP_DURATION }}
		>
			<div class="block-header">
				{#if section.block}
					<span class="block-ticker">{section.block.pool_ticker ?? section.block.pool_id?.slice(5, 10).toUpperCase()}</span>
				{:else}
					<span class="block-ticker">MEMPOOL</span>
				{/if}
			</div>

			{#if section.txs.length > 0}
				<BinPackGrid items={section.txs} key={(tx) => tx.hash} itemWidth={TX_WIDTH} gap={TX_GAP}>
					{#snippet children(tx)}
						<Transaction {tx} />
					{/snippet}
				</BinPackGrid>
			{/if}

			{#if section.block}
				<div class="block-footer">
					<span class="block-meta">#{section.block.number}</span>
					<span class="block-meta">
						{#if i === 1}{timeAgo(section.block.timestamp)}{:else}{formatTime(section.block.timestamp)}{/if}
					</span>
				</div>
			{:else}
				<div class="block-footer">&nbsp;</div>
			{/if}
		</div>
	{/each}
</div>

<style>
	.feed {
		flex: 1;
		overflow-y: auto;
		scrollbar-gutter: stable;
		padding: 16px 20px;
		display: flex;
		flex-direction: column;
		align-items: center;
	}

	.section {
		width: 100%;
		position: relative;
		border: var(--block-border) solid;
		border-radius: 8px;
		padding: var(--block-padding);
		display: flex;
		flex-direction: column;
	}

	.section.has-line::before {
		content: '';
		position: absolute;
		bottom: calc(100% + var(--block-border));
		left: 50%;
		width: 1px;
		height: var(--line-height);
		background: var(--border);
	}

	.section.mempool {
		filter: grayscale(1);
	}

	.block-header {
		text-align: center;
		margin-bottom: var(--block-padding);
	}

	.block-footer {
		display: flex;
		justify-content: space-between;
		margin-top: 8px;
	}

	.block-meta {
		color: white;
		font-size: 11px;
	}

	.block-ticker {
		color: white;
		font-size: 13px;
		font-weight: 700;
	}

</style>
