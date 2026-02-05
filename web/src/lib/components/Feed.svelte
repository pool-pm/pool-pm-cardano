<script lang="ts">
	import { mempoolTxs, blocks } from '../stores';
	import Transaction from './Transaction.svelte';
	import Block from './Block.svelte';
	import type { FeedTx, FeedBlock } from '../types';

	const PX_PER_SEC = 1;
	const MAX_AGE_MS = 600_000;

	let now = $state(Date.now());

	$effect(() => {
		let id: number;
		function tick() {
			now = Date.now();
			id = requestAnimationFrame(tick);
		}
		id = requestAnimationFrame(tick);
		return () => cancelAnimationFrame(id);
	});

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

	let items: FeedItem[] = $derived.by(() => {
		const result: FeedItem[] = [];

		for (const [hash, tx] of $mempoolTxs) {
			result.push({
				kind: 'tx',
				key: `tx-${hash}`,
				receivedAt: tx.receivedAt,
				data: tx,
			});
		}

		for (const [hash, block] of $blocks) {
			result.push({
				kind: 'block',
				key: `blk-${hash}`,
				receivedAt: block.receivedAt,
				data: block,
			});
		}

		result.sort((a, b) => a.receivedAt - b.receivedAt);
		return result;
	});
</script>

<div class="feed">
	{#each items as item (item.key)}
		{@const ageSec = (now - item.receivedAt) / 1000}
		<div
			class="feed-item"
			style="transform: translateY({ageSec * PX_PER_SEC}px)"
		>
			{#if item.kind === 'tx'}
				<Transaction tx={item.data} />
			{:else}
				<Block block={item.data} />
			{/if}
		</div>
	{/each}
</div>

<style>
	.feed {
		flex: 1;
		position: relative;
		overflow: hidden;
		padding: 16px 20px;
	}

	.feed-item {
		position: absolute;
		top: 0;
		left: 20px;
		right: 20px;
		will-change: transform;
	}
</style>
