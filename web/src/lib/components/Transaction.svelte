<script lang="ts">
	import type { FeedTx } from '../types';

	let { tx }: { tx: FeedTx } = $props();

	function truncateHash(h: string): string {
		return h.slice(0, 4) + '\u2026' + h.slice(-2);
	}

	function truncateAddr(a: string): string {
		if (a.startsWith('addr1') && a.length > 13) {
			return a.slice(0, 9) + '\u2026' + a.slice(-4);
		}
		if (a.startsWith('addr_test1') && a.length > 18) {
			return a.slice(0, 14) + '\u2026' + a.slice(-4);
		}
		if (a.length > 13) {
			return a.slice(0, 9) + '\u2026' + a.slice(-4);
		}
		return a;
	}

	function formatAda(lovelace: number): string {
		const ada = lovelace / 1_000_000;
		if (ada >= 1000) return Math.floor(ada).toLocaleString() + ' ADA';
		if (ada >= 1) return ada.toFixed(2) + ' ADA';
		return ada.toFixed(6) + ' ADA';
	}
</script>

<div class="tx-card">
	<div class="tx-hash mono">{truncateHash(tx.hash)}</div>

	<div class="addr-list">
		{#each tx.inputs.slice(0, 3) as input}
			<div class="addr-item">
				<span class="ada mono">{formatAda(input.lovelace)}</span>
				<span class="addr mono">{input.address ? truncateAddr(input.address) : '???'}</span>
			</div>
		{/each}
		{#if tx.inputs.length > 3}
			<div class="addr-item muted mono">+{tx.inputs.length - 3} more</div>
		{/if}
	</div>

	<div class="arrow">\u2193</div>

	<div class="addr-list">
		{#each tx.outputs.slice(0, 3) as output}
			<div class="addr-item">
				<span class="ada mono">{formatAda(output.lovelace)}</span>
				<span class="addr mono">{truncateAddr(output.address)}</span>
				{#if output.assets.length > 0}
					<div class="assets">
						{#each output.assets.slice(0, 4) as asset}
							<img
								class="asset-thumb"
								src="https://{asset.fingerprint}.preview.nftcdn.io/image?size=64"
								alt={asset.fingerprint}
								loading="lazy"
							/>
						{/each}
						{#if output.assets.length > 4}
							<span class="muted">+{output.assets.length - 4}</span>
						{/if}
					</div>
				{/if}
			</div>
		{/each}
		{#if tx.outputs.length > 3}
			<div class="addr-item muted mono">+{tx.outputs.length - 3} more</div>
		{/if}
	</div>
</div>

<style>
	.tx-card {
		background: var(--surface);
		border: 1px solid var(--border);
		border-radius: 6px;
		padding: 8px 10px;
		width: 180px;
		font-size: 11px;
	}

	.tx-hash {
		color: var(--accent);
		margin-bottom: 6px;
	}

	.addr-list {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}

	.addr-item {
		display: flex;
		flex-direction: column;
	}

	.ada {
		color: var(--positive);
		font-size: 10px;
	}

	.addr {
		color: var(--text);
		font-size: 10px;
	}

	.arrow {
		color: var(--text-muted);
		text-align: center;
		font-size: 12px;
		margin: 4px 0;
	}

	.muted {
		color: var(--text-muted);
		font-size: 10px;
	}

	.assets {
		display: flex;
		gap: 2px;
		margin-top: 2px;
	}

	.asset-thumb {
		width: 20px;
		height: 20px;
		border-radius: 3px;
		background: var(--bg);
	}
</style>
