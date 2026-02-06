<script lang="ts">
	import type { FeedTx, TxOutputInfo } from '../types';
	import { paymentCredential } from '../bech32';

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

	function formatAda(lovelace: string): string {
		const padded = lovelace.padStart(7, '0');
		const whole = padded.slice(0, -6) || '0';
		const frac = padded.slice(-6);
		const wholeNum = Number(whole);
		if (wholeNum >= 1000) return wholeNum.toLocaleString() + ' ADA';
		if (wholeNum >= 1) return whole + '.' + frac.slice(0, 2) + ' ADA';
		return '0.' + frac + ' ADA';
	}

	// Filter outputs: exclude those going back to a source address (change)
	let filteredOutputs: TxOutputInfo[] = $derived.by(() => {
		const inputPayments = new Set(
			tx.inputs
				.map((i) => (i.address ? paymentCredential(i.address) : null))
				.filter((x): x is string => x !== null)
		);
		return tx.outputs.filter((o) => {
			const cred = paymentCredential(o.address);
			return cred === null || !inputPayments.has(cred);
		});
	});

	// Count hidden change outputs
	let changeCount = $derived(tx.outputs.length - filteredOutputs.length);
</script>

<div class="tx-card">
	<div class="addr-list">
		{#each filteredOutputs as output}
			<div class="addr-item">
				<span class="ada mono">{formatAda(output.lovelace)}</span>
				<span class="addr mono">{truncateAddr(output.address)}</span>
				{#if output.assets.length > 0}
					<div class="assets">
						{#each output.assets as asset}
							<img
								class="asset-thumb"
								src="https://{asset.fingerprint}.preview.nftcdn.io/image?size=64"
								alt={asset.fingerprint}
								loading="lazy"
							/>
						{/each}
					</div>
				{/if}
			</div>
		{/each}
		{#if changeCount > 0 && filteredOutputs.length === 0}
			<div class="addr-item muted mono">({changeCount} change)</div>
		{/if}
	</div>

	<div class="arrow">↑</div>

	<div class="addr-list">
		{#each tx.inputs as input}
			<div class="addr-item">
				<span class="addr mono">{input.address ? truncateAddr(input.address) : '???'}</span>
			</div>
		{/each}
	</div>

	<div class="tx-hash mono">{truncateHash(tx.hash)}</div>
</div>

<style>
	.tx-card {
		background: var(--surface);
		border: 1px solid var(--border);
		border-radius: 6px;
		padding: 8px 10px;
		width: 180px;
		font-size: 11px;
		text-align: center;
	}

	.tx-hash {
		color: var(--accent);
		margin-top: 6px;
	}

	.addr-list {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 2px;
	}

	.addr-item {
		display: flex;
		flex-direction: column;
		align-items: center;
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
		justify-content: center;
	}

	.asset-thumb {
		width: 20px;
		height: 20px;
		border-radius: 3px;
		background: var(--bg);
	}
</style>
