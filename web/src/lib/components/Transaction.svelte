<script lang="ts">
	import type { AssetInfo, FeedTx, TxOutputInfo } from '../types';
	import { bech32Decode, paymentCredential, stakeCredential } from '../bech32';
	import { config } from '../stores';

	let { tx }: { tx: FeedTx } = $props();

	function truncateHash(h: string): string {
		return h.slice(0, 4) + '\u2026' + h.slice(-4);
	}

	function truncateAddr(a: string): string {
		const keep = a.startsWith('addr_test1') ? 14 : 9;
		return a.length > keep + 4 ? a.slice(0, keep) + '\u2026' + a.slice(-4) : a;
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

	function nftcdnUrl(asset: AssetInfo): string {
		const base = `https://${asset.fingerprint}.${$config!.nftcdn}/preview`;
		return asset.tk ? `${base}?tk=${asset.tk}&size=128` : `${base}?size=128`;
	}

	// Filter outputs: exclude those going back to a source address (change)
	let filteredOutputs: TxOutputInfo[] = $derived.by(() => {
		const inputPayments = new Set(
			tx.inputs
				.map((i) => (i.address ? paymentCredential(i.address) : null))
				.filter((x): x is string => x !== null)
		);
		const inputStakes = new Set(
			tx.inputs
				.map((i) => (i.address ? stakeCredential(i.address) : null))
				.filter((x): x is string => x !== null)
		);
		// First pass: filter by exact payment credential match
		const afterPayment = tx.outputs.filter((o) => {
			const cred = paymentCredential(o.address);
			return cred === null || !inputPayments.has(cred);
		});
		// Second pass: heuristic change detection for non-script addresses
		return afterPayment.filter((o) => {
			if (o.assets.length <= 4) return true;
			// Byron address with many assets, only if non-change outputs remain
			if (!o.address.startsWith('addr')) return afterPayment.length <= 1;
			// Same stake credential with many assets, but not a script address
			const bytes = bech32Decode(o.address);
			if (bytes && (bytes[0] & 0x10) !== 0) return true;
			const stake = stakeCredential(o.address);
			return stake === null || !inputStakes.has(stake);
		});
	});

	// Count hidden change outputs
	let changeCount = $derived(tx.outputs.length - filteredOutputs.length);

	// Total asset count across visible outputs → scale thumbnails
	let totalAssets = $derived(filteredOutputs.reduce((sum, o) => sum + o.assets.length, 0));
	let thumbSize = $derived(totalAssets <= 1 ? 64 : Math.max(16, Math.floor(64 / Math.sqrt(totalAssets))));

	// Deduplicate inputs by address
	let uniqueInputs = $derived(
		[...new Map(tx.inputs.map((i) => [i.address, i])).values()]
	);
</script>

<div class="tx-card" style:--thumb-size="{thumbSize}px">
	<div class="addr-list">
		{#each filteredOutputs as output}
			<div class="addr-item">
				<span class="ada">{formatAda(output.lovelace)}</span>
				<span class="addr mono">{truncateAddr(output.address)}</span>
				{#if output.assets.length > 0 && $config}
					<div class="assets">
						{#each output.assets as asset}
							<div class="asset">
								<img
									class="asset-thumb"
									src={nftcdnUrl(asset)}
									alt={asset.fingerprint}
									loading="lazy"
									onerror={(e: Event) => {
									const asset = (e.target as HTMLElement).parentElement!;
									const parent = asset.parentElement;
									asset.remove();
									parent?.dispatchEvent(new Event('remeasure', { bubbles: true }));
								}}
								/>
								{#if thumbSize >= 32 && asset.quantity !== '1'}
									<span class="asset-label">{BigInt(asset.quantity).toLocaleString()}</span>
								{/if}
							</div>
						{/each}
					</div>
				{/if}
			</div>
		{/each}
		</div>

	{#if filteredOutputs.length === 0}
		<span class="ada">{formatAda(tx.outputs.reduce((s, o) => s + BigInt(o.lovelace), 0n).toString())}</span>
	{/if}
	<div class="arrow" class:flip={filteredOutputs.length === 0}>{filteredOutputs.length === 0 ? '↻' : '↑'}</div>

	<div class="addr-list">
		{#each uniqueInputs as input}
			<div class="addr-item">
				<span class="addr mono">{input.address ? truncateAddr(input.address) : '???'}</span>
			</div>
		{/each}
	</div>

	<div class="tx-hash mono">{truncateHash(tx.hash)}</div>
</div>

<style>
	.tx-card {
		background: rgb(0 0 0 / 0.5);
		border-radius: 6px;
		padding: 8px 10px;
		width: var(--item-width);
		font-size: 11px;
		text-align: center;
		transition: filter var(--flip-duration) ease;
	}

	.tx-hash {
		color: var(--section-color, var(--accent));
		font-size: 10px;
		margin-top: 6px;
	}

	.addr-list {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 4px;
	}

	.addr-item {
		display: flex;
		flex-direction: column;
		align-items: center;
	}

	.ada {
		color: white;
		font-weight: 600;
		font-size: 10px;
	}

	.addr {
		color: rgb(255 255 255 / 0.4);
		font-size: 10px;
	}

	.arrow {
		color: rgb(255 255 255 / 0.4);
		text-align: center;
		font-size: 12px;
		margin: 4px 0;
	}

	.arrow.flip {
		transform: rotate(120deg);
	}

	.muted {
		color: var(--text-muted);
		font-size: 10px;
	}

	.assets {
		display: flex;
		flex-wrap: wrap;
		gap: 2px;
		margin-top: 2px;
		justify-content: center;
	}

	.asset {
		display: flex;
		flex-direction: column;
		align-items: center;
	}

	.asset-label {
		font-size: 9px;
		color: white;
		text-align: center;
		white-space: nowrap;
	}

	.asset-thumb {
		width: var(--thumb-size, 64px);
		border-radius: 3px;
		background: transparent;
	}
</style>
