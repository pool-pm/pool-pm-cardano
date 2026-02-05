<script lang="ts">
	import type { FeedTx } from '../types';

	let { tx }: { tx: FeedTx } = $props();

	function truncate(s: string, len = 12): string {
		if (s.length <= len) return s;
		return s.slice(0, len / 2) + '...' + s.slice(-len / 2);
	}

	function formatAda(lovelace: number): string {
		return (lovelace / 1_000_000).toFixed(2);
	}

	const totalIn = $derived(
		tx.inputs.reduce((sum, i) => sum + i.lovelace, 0),
	);
	const totalOut = $derived(
		tx.outputs.reduce((sum, o) => sum + o.lovelace, 0),
	);
	const allAssets = $derived(tx.outputs.flatMap((o) => o.assets));
</script>

<div class="tx-card">
	<div class="tx-header">
		<span class="tx-hash mono">{truncate(tx.hash, 16)}</span>
		<span class="tx-fee mono">{formatAda(tx.fee)} fee</span>
	</div>

	<div class="tx-flow">
		<div class="tx-inputs">
			{#each tx.inputs.slice(0, 3) as input}
				<div class="addr mono">
					{input.address ? truncate(input.address, 20) : '???'}
				</div>
			{/each}
			{#if tx.inputs.length > 3}
				<div class="addr mono muted">+{tx.inputs.length - 3} more</div>
			{/if}
			<div class="amount">{formatAda(totalIn)} ADA</div>
		</div>

		<div class="arrow">&#x2192;</div>

		<div class="tx-outputs">
			{#each tx.outputs.slice(0, 3) as output}
				<div class="addr mono">
					{truncate(output.address, 20)}
				</div>
			{/each}
			{#if tx.outputs.length > 3}
				<div class="addr mono muted">
					+{tx.outputs.length - 3} more
				</div>
			{/if}
			<div class="amount">{formatAda(totalOut)} ADA</div>
		</div>
	</div>

	{#if allAssets.length > 0}
		<div class="tx-assets">
			{#each allAssets.slice(0, 8) as asset}
				<img
					class="asset-thumb"
					src="https://{asset.fingerprint}.preview.nftcdn.io/image?size=64"
					alt={asset.fingerprint}
					loading="lazy"
				/>
			{/each}
			{#if allAssets.length > 8}
				<span class="muted">+{allAssets.length - 8}</span>
			{/if}
		</div>
	{/if}
</div>

<style>
	.tx-card {
		background: var(--surface);
		border: 1px solid var(--border);
		border-radius: 8px;
		padding: 10px 14px;
		margin-bottom: 8px;
	}

	.tx-header {
		display: flex;
		justify-content: space-between;
		align-items: center;
		margin-bottom: 8px;
	}

	.tx-hash {
		color: var(--accent);
	}

	.tx-fee {
		color: var(--text-muted);
		font-size: 11px;
	}

	.tx-flow {
		display: flex;
		align-items: center;
		gap: 12px;
		font-size: 12px;
	}

	.tx-inputs,
	.tx-outputs {
		flex: 1;
		min-width: 0;
	}

	.addr {
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		line-height: 1.6;
	}

	.amount {
		color: var(--positive);
		font-weight: 600;
		font-size: 13px;
		margin-top: 4px;
	}

	.arrow {
		color: var(--text-muted);
		font-size: 18px;
		flex-shrink: 0;
	}

	.muted {
		color: var(--text-muted);
	}

	.tx-assets {
		display: flex;
		gap: 4px;
		align-items: center;
		margin-top: 8px;
		flex-wrap: wrap;
	}

	.asset-thumb {
		width: 32px;
		height: 32px;
		border-radius: 4px;
		background: var(--bg);
	}
</style>
